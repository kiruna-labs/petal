<!--
  Dev-only visual QA harness for Settings (DESIGN.md §9). Renders the panel
  with realistic sample data, plus a couple of permission-state variants so
  the "re-entry" flow (denied -> recovering) can be sanity-checked. Throwaway
  scaffolding, matches the /dev/components pattern.
-->
<script lang="ts">
  import Settings from '$lib/components/Settings.svelte';
</script>

<div class="harness">
  <h1>Petal — settings dev harness</h1>
  <p class="intro">Settings panel, per DESIGN.md §9. Placeholder-pending-design (functional-but-plain). Dev-only route.</p>

  <div class="row">
    <div class="cell">
      <Settings
        userName="Jordan Kim"
        identity="plum"
        cameras={[
          { id: 'builtin', label: 'FaceTime HD Camera' },
          { id: 'external', label: 'Logitech Brio' }
        ]}
        mics={[
          { id: 'builtin', label: 'MacBook Pro Microphone' },
          { id: 'usb', label: 'Shure MV7' }
        ]}
        speakers={[
          { id: 'default', label: 'MacBook Pro Speakers' },
          { id: 'headphones', label: 'AirPods Pro' }
        ]}
        screenRecordingStatus="enabled"
        micStatus="enabled"
        cameraStatus="enabled"
      />
      <span class="caption">All permissions already granted (typical re-entry view)</span>
    </div>

    <div class="cell">
      <Settings
        userName="Jordan Kim"
        identity="blue"
        screenRecordingStatus="denied"
        micStatus="up-next"
        cameraStatus="optional"
      />
      <span class="caption">Screen Recording denied — recovery card re-shown from Settings (SPEC.md §4.1)</span>
    </div>

    <div class="cell">
      <Settings
        userName="Jordan Kim"
        identity="green"
        screenRecordingStatus="enabled"
        micStatus="enabled"
        cameraStatus="denied"
      />
      <span class="caption">Camera denied (macOS TCC, issue #8) — preview shows the System Settings recovery path, camera row expands to the denied card</span>
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
