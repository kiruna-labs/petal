<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { COMMANDS } from '$lib/ipc';
  import Button from './Button.svelte';

  type RunStatus = 'running' | 'passed' | 'failed' | 'cancelled' | 'error' | 'unknown' | string;

  interface TestCockpitEvent {
    kind: string;
    scenarioId?: string | null;
    payload?: unknown;
  }

  interface TestCockpitArtifact {
    type: string;
    path: string;
    stepId?: string | null;
    tMs?: number | null;
  }

  interface TestCockpitRunSummary {
    runId: string;
    resultsDir?: string | null;
    updatedAtUnixMs?: number | null;
    status?: RunStatus | null;
    pass?: number | null;
    fail?: number | null;
    skipped?: number | null;
    parseErrors?: number | null;
    conclusion?: unknown;
  }

  interface TestCockpitRun extends TestCockpitRunSummary {
    events?: TestCockpitEvent[];
    artifacts?: TestCockpitArtifact[];
    scorecard?: unknown;
  }

  interface Props {
    runs?: TestCockpitRun[];
    selectedRun?: TestCockpitRun | string | null;
    loading?: boolean;
    error?: string | null;
    onRefresh?: () => void;
    onSelectRun?: (runId: string) => void;
    onOpenFolder?: (path: string) => void;
  }

  let {
    runs = [],
    selectedRun = null,
    loading = false,
    error = null,
    onRefresh,
    onSelectRun,
    onOpenFolder
  }: Props = $props();

  const selectedRunId = $derived(
    typeof selectedRun === 'string' ? selectedRun : (selectedRun?.runId ?? null)
  );
  const activeRun = $derived.by(() => {
    if (typeof selectedRun === 'object' && selectedRun) return selectedRun;
    return runs.find((run) => run.runId === selectedRunId) ?? runs[0] ?? null;
  });
  const sortedRuns = $derived(
    [...runs].sort((a, b) => (b.updatedAtUnixMs ?? 0) - (a.updatedAtUnixMs ?? 0))
  );
  const activeEvents = $derived(activeRun?.events ?? []);
  const activeArtifacts = $derived(activeRun?.artifacts ?? []);
  const previewArtifacts = $derived(
    activeArtifacts.filter((artifact) => artifact.type?.toLowerCase() === 'screenshot')
  );
  const videoArtifacts = $derived(
    activeArtifacts.filter((artifact) => ['video', 'recording'].includes(artifact.type?.toLowerCase()))
  );
  const audioArtifacts = $derived(
    activeArtifacts.filter((artifact) => artifact.type?.toLowerCase() === 'audio')
  );
  const playableArtifacts = $derived([...previewArtifacts, ...videoArtifacts, ...audioArtifacts]);
  let artifactSources = $state<Record<string, string>>({});
  let artifactSourceErrors = $state<Record<string, string>>({});

  function formatTime(ms?: number | null) {
    if (!ms) return '—';
    return new Intl.DateTimeFormat(undefined, {
      month: 'short',
      day: 'numeric',
      hour: 'numeric',
      minute: '2-digit'
    }).format(new Date(ms));
  }

  function formatOffset(ms?: number | null) {
    if (ms === null || ms === undefined || Number.isNaN(ms)) return '—';
    if (ms < 1000) return `${Math.round(ms)} ms`;
    return `${(ms / 1000).toFixed(ms < 10_000 ? 1 : 0)} s`;
  }

  function shortRunId(runId: string) {
    if (runId.length <= 18) return runId;
    return `${runId.slice(0, 8)}…${runId.slice(-6)}`;
  }

  function summarizePayload(payload: unknown) {
    if (payload === null || payload === undefined) return '';
    if (typeof payload === 'string') return payload;
    if (typeof payload === 'number' || typeof payload === 'boolean') return String(payload);
    try {
      return JSON.stringify(payload);
    } catch {
      return String(payload);
    }
  }

  function scorecardRows(scorecard: unknown) {
    if (!scorecard || typeof scorecard !== 'object' || Array.isArray(scorecard)) return [];
    return Object.entries(scorecard as Record<string, unknown>).map(([key, value]) => ({
      key,
      value: summarizePayload(value)
    }));
  }

  function handleOpenFolder(path?: string | null) {
    if (!path) return;
    onOpenFolder?.(path);
  }

  function artifactKey(run: TestCockpitRun | null, artifact: TestCockpitArtifact, index: number) {
    return `${run?.runId ?? 'no-run'}-${run?.resultsDir ?? 'no-dir'}-${artifact.path}-${artifact.stepId ?? ''}-${index}`;
  }

  async function loadArtifactPreview(
    runId: string,
    resultsDir: string,
    artifact: TestCockpitArtifact,
    key: string
  ) {
    try {
      const source = await invoke<string>(COMMANDS.getTestCockpitArtifactDataUrl, {
        resultsDir,
        path: artifact.path
      });
      if (activeRun?.runId !== runId || activeRun?.resultsDir !== resultsDir) return;
      artifactSources = { ...artifactSources, [key]: source };
    } catch (error) {
      if (activeRun?.runId !== runId || activeRun?.resultsDir !== resultsDir) return;
      artifactSourceErrors = {
        ...artifactSourceErrors,
        [key]: String(error)
      };
    }
  }

  $effect(() => {
    const run = activeRun;
    const artifacts = playableArtifacts;
    artifactSources = {};
    artifactSourceErrors = {};
    if (!run?.resultsDir) return;
    artifacts.forEach((artifact, index) => {
      void loadArtifactPreview(run.runId, run.resultsDir!, artifact, artifactKey(run, artifact, index));
    });
  });
</script>

<section class="results-viewer" aria-label="Test cockpit results">
  <header class="viewer-head">
    <div class="head-copy">
      <h2>Results</h2>
      <span>{loading ? 'Refreshing...' : `${runs.length} run${runs.length === 1 ? '' : 's'}`}</span>
    </div>
    <Button variant="ghost" disabled={loading} onclick={() => onRefresh?.()}>
      Refresh
    </Button>
  </header>

  {#if error}
    <p class="note error">{error}</p>
  {/if}

  <div class="viewer-grid">
    <aside class="run-list" aria-label="Runs">
      {#if sortedRuns.length === 0}
        <p class="empty">No saved runs.</p>
      {:else}
        {#each sortedRuns as run (run.runId)}
          <button
            type="button"
            class="run-row"
            class:active={activeRun?.runId === run.runId}
            onclick={() => onSelectRun?.(run.runId)}
            aria-pressed={activeRun?.runId === run.runId}
          >
            <span class="run-main">
              <span class="run-id" title={run.runId}>{shortRunId(run.runId)}</span>
              <span class={`status status-${run.status ?? 'unknown'}`}>{run.status ?? 'unknown'}</span>
            </span>
            <span class="run-meta">{formatTime(run.updatedAtUnixMs)}</span>
            <span class="run-counts">
              <b>{run.pass ?? 0}</b> pass
              <b>{run.fail ?? 0}</b> fail
              <b>{run.skipped ?? 0}</b> skip
            </span>
          </button>
        {/each}
      {/if}
    </aside>

    <div class="detail-pane">
      {#if !activeRun}
        <p class="empty">Select a run to inspect results.</p>
      {:else}
        <section class="detail-section summary" aria-label="Run summary">
          <div class="summary-title">
            <div>
              <h3 title={activeRun.runId}>{activeRun.runId}</h3>
              <span>{formatTime(activeRun.updatedAtUnixMs)}</span>
            </div>
            <span class={`status status-${activeRun.status ?? 'unknown'}`}>
              {activeRun.status ?? 'unknown'}
            </span>
          </div>

          <div class="metric-grid">
            <span><b>{activeRun.pass ?? 0}</b> passed</span>
            <span><b>{activeRun.fail ?? 0}</b> failed</span>
            <span><b>{activeRun.skipped ?? 0}</b> skipped</span>
          </div>

          {#if activeRun.parseErrors}
            <p class="note error">{activeRun.parseErrors} result line{activeRun.parseErrors === 1 ? '' : 's'} could not be parsed.</p>
          {/if}

          <div class="folder-row">
            <span title={activeRun.resultsDir ?? ''}>{activeRun.resultsDir ?? 'Results folder unavailable'}</span>
            <Button
              variant="ghost"
              disabled={!activeRun.resultsDir}
              onclick={() => handleOpenFolder(activeRun.resultsDir)}
            >
              Open folder
            </Button>
          </div>
        </section>

        {#if activeRun.conclusion}
          <section class="detail-section conclusion" aria-label="Conclusion">
            <div class="section-head">
              <h3>Conclusion</h3>
            </div>
            {#if typeof activeRun.conclusion === 'object' && activeRun.conclusion !== null && !Array.isArray(activeRun.conclusion)}
              {@const conclusion = activeRun.conclusion as Record<string, unknown>}
              <p class:conclusion-aborted={conclusion.status === 'aborted'}>{conclusion.message ?? conclusion.status}</p>
              {#if Array.isArray(conclusion.scenarios)}
                <ul class="conclusion-list">
                  {#each conclusion.scenarios as scenario}
                    <li>{summarizePayload(scenario)}</li>
                  {/each}
                </ul>
              {/if}
              {#if Array.isArray(conclusion.notChecked) && conclusion.notChecked.length > 0}
                <p><strong>Not checked:</strong> {summarizePayload(conclusion.notChecked)}</p>
              {/if}
            {:else}
              <p>{summarizePayload(activeRun.conclusion)}</p>
            {/if}
          </section>
        {/if}

        <section class="detail-section" aria-label="Artifacts">
          <div class="section-head">
            <h3>Artifacts</h3>
            <span>{activeArtifacts.length}</span>
          </div>
          {#if activeArtifacts.length === 0}
            <p class="empty">No artifacts recorded.</p>
          {:else}
            {#if previewArtifacts.length > 0}
              <div class="artifact-gallery" aria-label="Screenshot artifacts">
                {#each previewArtifacts as artifact, i (artifactKey(activeRun, artifact, i))}
                  {@const key = artifactKey(activeRun, artifact, i)}
                  <figure class="artifact-preview">
                    <div class="artifact-image-frame">
                      {#if artifactSources[key]}
                        <img src={artifactSources[key]} alt={`${artifact.stepId ?? 'screenshot'} artifact`} />
                      {:else if artifactSourceErrors[key]}
                        <span>{artifactSourceErrors[key]}</span>
                      {:else}
                        <span>Loading preview...</span>
                      {/if}
                    </div>
                    <figcaption>
                      <span>{artifact.stepId ?? 'screenshot'}</span>
                      <span title={artifact.path}>{artifact.path}</span>
                    </figcaption>
                  </figure>
                {/each}
              </div>
            {/if}
            {#if videoArtifacts.length > 0}
              <div class="media-stack" aria-label="Video artifacts">
                {#each videoArtifacts as artifact, i (artifactKey(activeRun, artifact, i))}
                  {@const key = artifactKey(activeRun, artifact, i)}
                  <figure class="media-preview">
                    <div class="media-frame">
                      {#if artifactSources[key]}
                        <video src={artifactSources[key]} controls preload="metadata" playsinline>
                          <track kind="captions" />
                        </video>
                      {:else if artifactSourceErrors[key]}
                        <span>{artifactSourceErrors[key]}</span>
                      {:else}
                        <span>Loading video...</span>
                      {/if}
                    </div>
                    <figcaption>
                      <span>{artifact.stepId ?? 'video'}</span>
                      <span title={artifact.path}>{artifact.path}</span>
                    </figcaption>
                  </figure>
                {/each}
              </div>
            {/if}
            {#if audioArtifacts.length > 0}
              <div class="media-stack" aria-label="Audio artifacts">
                {#each audioArtifacts as artifact, i (artifactKey(activeRun, artifact, i))}
                  {@const key = artifactKey(activeRun, artifact, i)}
                  <figure class="audio-preview">
                    <figcaption>
                      <span>{artifact.stepId ?? 'audio'}</span>
                      <span title={artifact.path}>{artifact.path}</span>
                    </figcaption>
                    {#if artifactSources[key]}
                      <audio src={artifactSources[key]} controls preload="metadata"></audio>
                    {:else if artifactSourceErrors[key]}
                      <span class="media-error">{artifactSourceErrors[key]}</span>
                    {:else}
                      <span class="media-error">Loading audio...</span>
                    {/if}
                  </figure>
                {/each}
              </div>
            {/if}
            <div class="artifact-list">
              {#each activeArtifacts as artifact, i (`${artifact.path}-${artifact.stepId ?? i}`)}
                <div class="artifact-row">
                  <span class="artifact-type">{artifact.type}</span>
                  <span class="artifact-path" title={artifact.path}>{artifact.path}</span>
                  <span>{artifact.stepId ?? '—'}</span>
                  <span>{formatOffset(artifact.tMs)}</span>
                </div>
              {/each}
            </div>
          {/if}
        </section>

        <section class="detail-section" aria-label="Event timeline">
          <div class="section-head">
            <h3>Timeline</h3>
            <span>{activeEvents.length}</span>
          </div>
          {#if activeEvents.length === 0}
            <p class="empty">No events recorded.</p>
          {:else}
            <ol class="timeline">
              {#each activeEvents as event, i (`${event.kind}-${event.scenarioId ?? ''}-${i}`)}
                <li>
                  <span class="event-kind">{event.kind}</span>
                  <span class="event-scenario">{event.scenarioId ?? 'run'}</span>
                  {#if summarizePayload(event.payload)}
                    <span class="event-payload">{summarizePayload(event.payload)}</span>
                  {/if}
                </li>
              {/each}
            </ol>
          {/if}
        </section>

        {#if activeRun.scorecard}
          <section class="detail-section" aria-label="Scorecard">
            <div class="section-head">
              <h3>Scorecard</h3>
            </div>
            {#if scorecardRows(activeRun.scorecard).length > 0}
              <dl class="scorecard">
                {#each scorecardRows(activeRun.scorecard) as row (row.key)}
                  <div>
                    <dt>{row.key}</dt>
                    <dd>{row.value}</dd>
                  </div>
                {/each}
              </dl>
            {:else}
              <pre class="scorecard-raw">{summarizePayload(activeRun.scorecard)}</pre>
            {/if}
          </section>
        {/if}
      {/if}
    </div>
  </div>
</section>

<style>
  .results-viewer {
    display: flex;
    min-height: 0;
    flex-direction: column;
    gap: 10px;
    color: var(--text-primary);
    font-family: var(--font-ui);
  }

  .viewer-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
  }

  .head-copy {
    display: flex;
    min-width: 0;
    flex-direction: column;
    gap: 3px;
  }

  h2,
  h3,
  p {
    margin: 0;
  }

  h2 {
    font: 600 11px var(--font-mono);
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--text-faint);
  }

  h3 {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font: 650 13px var(--font-ui);
    color: var(--text-primary);
  }

  .head-copy span,
  .section-head span,
  .summary-title span,
  .note,
  .empty {
    font: 500 10px/1.35 var(--font-mono);
    color: var(--text-faint);
  }

  .viewer-head :global(.btn),
  .folder-row :global(.btn) {
    height: 32px;
    padding: 0 10px;
    font-size: 11px;
    flex-shrink: 0;
  }

  .viewer-grid {
    display: grid;
    grid-template-columns: minmax(168px, 0.74fr) minmax(0, 1.6fr);
    gap: 10px;
    min-height: 0;
  }

  .run-list,
  .detail-pane {
    min-height: 0;
    overflow: auto;
  }

  .run-list {
    display: flex;
    flex-direction: column;
    gap: 6px;
    max-height: min(56vh, 520px);
  }

  .run-row {
    display: flex;
    width: 100%;
    min-width: 0;
    flex-direction: column;
    gap: 5px;
    padding: 9px;
    border: 0;
    border-radius: var(--radius-chip);
    background: var(--fill-weak);
    box-shadow: var(--shadow-inset-hairline);
    color: inherit;
    text-align: left;
    cursor: pointer;
  }

  .run-row:hover,
  .run-row.active {
    background: var(--fill-base);
  }

  .run-row:focus-visible {
    outline: 2px solid var(--id-blue);
    outline-offset: 2px;
  }

  .run-main,
  .summary-title,
  .section-head,
  .folder-row,
  .artifact-row {
    display: flex;
    align-items: center;
    min-width: 0;
    gap: 8px;
  }

  .run-main,
  .summary-title,
  .section-head,
  .folder-row {
    justify-content: space-between;
  }

  .run-id,
  .artifact-path,
  .folder-row span,
  .event-payload,
  dd {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .run-id {
    font: 700 11px var(--font-mono);
    color: var(--text-primary);
  }

  .run-meta,
  .run-counts {
    font: 600 10px var(--font-mono);
    color: var(--text-faint);
  }

  .run-counts {
    display: flex;
    gap: 6px;
    color: var(--text-muted);
  }

  .run-counts b,
  .metric-grid b {
    color: var(--text-primary);
    font-variant-numeric: tabular-nums;
  }

  .status {
    flex-shrink: 0;
    padding: 2px 6px;
    border-radius: var(--radius-chip);
    background: var(--fill-base);
    color: var(--text-faint);
    font: 700 9.5px var(--font-mono);
    text-transform: uppercase;
  }

  .status-passed {
    background: rgba(52, 199, 89, 0.12);
    color: var(--live-bright);
  }

  .status-failed,
  .status-error {
    background: rgba(255, 69, 58, 0.12);
    color: var(--danger);
  }

  .status-running {
    background: rgba(110, 139, 255, 0.12);
    color: var(--id-blue);
  }

  .detail-pane {
    display: flex;
    flex-direction: column;
    gap: 10px;
    max-height: min(56vh, 520px);
  }

  .detail-section {
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 10px;
    border-radius: var(--radius-tile);
    background: var(--fill-weak);
    box-shadow: var(--shadow-inset-hairline);
  }

  .summary-title > div {
    display: flex;
    min-width: 0;
    flex-direction: column;
    gap: 3px;
  }

  .metric-grid {
    display: grid;
    grid-template-columns: repeat(3, minmax(0, 1fr));
    gap: 6px;
  }

  .metric-grid span {
    min-width: 0;
    padding: 7px 8px;
    border-radius: var(--radius-chip);
    background: var(--fill-weak);
    font: 600 10px var(--font-mono);
    color: var(--text-muted);
  }

  .folder-row {
    gap: 10px;
    padding-top: 2px;
  }

  .folder-row span {
    font: 500 10px var(--font-mono);
    color: var(--text-faint);
  }

  .artifact-list,
  .artifact-gallery,
  .media-stack,
  .timeline {
    display: flex;
    flex-direction: column;
    gap: 5px;
    margin: 0;
    padding: 0;
  }

  .artifact-gallery {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
    gap: 8px;
  }

  .artifact-preview {
    display: flex;
    min-width: 0;
    flex-direction: column;
    gap: 6px;
    margin: 0;
    padding: 8px;
    border-radius: var(--radius-chip);
    background: var(--fill-weak);
    box-shadow: var(--shadow-inset-hairline);
  }

  .media-stack {
    gap: 8px;
  }

  .media-preview,
  .audio-preview {
    display: flex;
    min-width: 0;
    flex-direction: column;
    gap: 7px;
    margin: 0;
    padding: 8px;
    border-radius: var(--radius-chip);
    background: var(--fill-weak);
    box-shadow: var(--shadow-inset-hairline);
  }

  .artifact-image-frame {
    display: grid;
    min-height: 112px;
    aspect-ratio: 16 / 10;
    place-items: center;
    overflow: hidden;
    border-radius: var(--radius-chip);
    background: rgba(0, 0, 0, 0.28);
  }

  .media-frame {
    display: grid;
    min-height: 156px;
    aspect-ratio: 16 / 9;
    place-items: center;
    overflow: hidden;
    border-radius: var(--radius-chip);
    background: rgba(0, 0, 0, 0.32);
  }

  .artifact-image-frame img {
    width: 100%;
    height: 100%;
    object-fit: contain;
  }

  .media-frame video {
    width: 100%;
    height: 100%;
    object-fit: contain;
  }

  .audio-preview audio {
    width: 100%;
  }

  .artifact-image-frame span,
  .media-frame span,
  .media-error,
  .artifact-preview figcaption,
  .media-preview figcaption,
  .audio-preview figcaption {
    font: 600 10px var(--font-mono);
    color: var(--text-faint);
  }

  .artifact-preview figcaption,
  .media-preview figcaption,
  .audio-preview figcaption {
    display: grid;
    min-width: 0;
    grid-template-columns: minmax(58px, 0.65fr) minmax(0, 1.35fr);
    gap: 6px;
  }

  .artifact-preview figcaption span,
  .media-preview figcaption span,
  .audio-preview figcaption span {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .artifact-row {
    display: grid;
    grid-template-columns: minmax(58px, 0.55fr) minmax(0, 1.7fr) minmax(52px, 0.55fr) minmax(48px, 0.45fr);
    padding: 7px 8px;
    border-radius: var(--radius-chip);
    background: var(--fill-weak);
    font: 600 10px var(--font-mono);
    color: var(--text-faint);
  }

  .artifact-type {
    color: var(--text-muted);
    text-transform: uppercase;
  }

  .artifact-path {
    color: var(--text-primary);
  }

  .timeline {
    list-style: none;
  }

  .timeline li {
    display: grid;
    grid-template-columns: minmax(86px, 0.7fr) minmax(72px, 0.6fr) minmax(0, 1.7fr);
    gap: 8px;
    padding: 7px 8px;
    border-radius: var(--radius-chip);
    background: var(--fill-weak);
    font: 500 10.5px/1.35 var(--font-ui);
    color: var(--text-muted);
  }

  .event-kind,
  .event-scenario {
    font: 700 10px var(--font-mono);
    color: var(--text-primary);
  }

  .event-scenario {
    color: var(--text-faint);
  }

  .scorecard {
    display: flex;
    flex-direction: column;
    gap: 5px;
    margin: 0;
  }

  .conclusion p,
  .conclusion-list {
    margin: 0;
    overflow-wrap: anywhere;
  }

  .conclusion-aborted {
    color: var(--danger);
    font-weight: 650;
  }

  .conclusion-list {
    display: grid;
    gap: 4px;
    padding-left: 18px;
  }

  .scorecard div {
    display: grid;
    grid-template-columns: minmax(84px, 0.6fr) minmax(0, 1.4fr);
    gap: 8px;
    padding: 7px 8px;
    border-radius: var(--radius-chip);
    background: var(--fill-weak);
  }

  dt,
  dd,
  .scorecard-raw {
    margin: 0;
    font: 600 10px var(--font-mono);
  }

  dt {
    color: var(--text-faint);
  }

  dd,
  .scorecard-raw {
    color: var(--text-muted);
  }

  .scorecard-raw {
    overflow: auto;
    padding: 8px;
    border-radius: var(--radius-chip);
    background: var(--fill-weak);
  }

  .note.error {
    color: var(--danger);
  }

  @media (max-width: 620px) {
    .viewer-grid {
      grid-template-columns: 1fr;
    }

    .run-list,
    .detail-pane {
      max-height: none;
    }

    .artifact-row,
    .timeline li,
    .scorecard div {
      grid-template-columns: 1fr;
    }
  }
</style>
