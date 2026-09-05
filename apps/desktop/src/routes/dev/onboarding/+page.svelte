<!--
  Dev-only visual QA harness for Onboarding (Petal-Build-Map.md §2.7).
  Renders the single-view checklist in a few of its states — mid-progress,
  a permission-denied+recovery state, and the fully-ready state — matching
  canvas.html §8a/8b/8c. Throwaway scaffolding, matches the /dev/components
  pattern.
-->
<script lang="ts">
  import Onboarding from '$lib/components/Onboarding.svelte';
</script>

<div class="harness">
  <h1>Petal — onboarding dev harness</h1>
  <p class="intro">Onboarding single-view checklist, per Petal-Build-Map.md §2.7. Dev-only route.</p>

  <div class="row">
    <div class="cell">
      <Onboarding
        screenRecordingStatus="in-progress"
        micStatus="up-next"
        cameraStatus="up-next"
        onOpenSettings={() => console.log('open system settings')}
      />
      <span class="caption">8a — in progress (0 of 3 ready, mic and camera required)</span>
    </div>

    <div class="cell">
      <Onboarding
        screenRecordingStatus="denied"
        micStatus="blocked"
        cameraStatus="blocked"
        onOpenSettings={() => console.log('open screen recording privacy')}
      />
      <span class="caption">8b — denied, inline recovery shown (blocked until above)</span>
    </div>

    <div class="cell">
      <Onboarding
        screenRecordingStatus="enabled"
        micStatus="enabled"
        cameraStatus="enabled"
        onContinue={() => console.log('continue to identity')}
      />
      <span class="caption">ready — continue to separate identity screen</span>
    </div>

    <div class="cell">
      <Onboarding
        screenRecordingStatus="enabled"
        micStatus="enabled"
        cameraStatus="denied"
        onRequestCamera={() => console.log('open camera privacy and recheck')}
      />
      <span class="caption">camera denied — required gate blocks onboarding with recovery</span>
    </div>
  </div>
</div>

<style>
  .harness {
    min-height: 100%;
    background: var(--bg-base);
    color: var(--text-primary);
    font-family: var(--font-ui);
    padding: 32px 40px 80px;
    overflow-y: auto;
  }

  h1 {
    font-family: var(--font-display);
    font-weight: 700;
    font-size: 28px;
    margin: 0 0 4px;
  }

  .intro {
    color: var(--text-muted);
    font-size: var(--text-body);
    margin: 0 0 32px;
  }

  .row {
    display: flex;
    flex-wrap: wrap;
    gap: 40px;
    align-items: flex-start;
  }

  .cell {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 12px;
  }

  .caption {
    font-size: 10.5px;
    font-family: var(--font-mono);
    color: var(--text-muted);
    text-align: center;
    max-width: 380px;
  }
</style>
