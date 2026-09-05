<!--
  Dev-only visual QA harness for the remaining DESIGN.md §9 secondary
  surfaces: RosterPopover, Toast variants, and the standard empty/offline
  states. Sibling to /dev/settings (Settings got its own route since it's
  the larger surface); this one groups the smaller presentational pieces,
  matching how prior phases split /dev/main-menu vs /dev/onboarding.
  Throwaway scaffolding, matches the /dev/components pattern.
-->
<script lang="ts">
  import RosterPopover from '$lib/components/RosterPopover.svelte';
  import type { RosterParticipant } from '$lib/components/RosterPopover.svelte';
  import Toast from '@petal/shared/ui/components/Toast.svelte';
  import EmptyState from '$lib/components/EmptyState.svelte';
  import OfflineState from '$lib/components/OfflineState.svelte';

  const rosterFull: RosterParticipant[] = [
    { name: 'Jordan Kim', identity: 'plum', isYou: true },
    { name: 'Marco', identity: 'blue', speaking: true },
    { name: 'Devin', identity: 'lilac', muted: true },
    { name: 'Sana', identity: 'green' },
    { name: 'Priya', identity: 'amber', muted: true }
  ];
</script>

<div class="harness">
  <h1>Petal — secondary surfaces dev harness</h1>
  <p class="intro">
    RosterPopover, Toast, and standard empty/offline states — DESIGN.md §9. All
    placeholder-pending-design (functional-but-plain). Dev-only route.
  </p>

  <!-- ============================================================ -->
  <section>
    <h2>RosterPopover</h2>
    <div class="row">
      <div class="cell">
        <RosterPopover roomName="eng-sync" participants={rosterFull} />
        <span class="caption">Populated roster — identity rings, speaking + muted indicators</span>
      </div>
      <div class="cell">
        <RosterPopover roomName="design-review" participants={[]} />
        <span class="caption">Empty roster (reuses EmptyState)</span>
      </div>
    </div>
  </section>

  <!-- ============================================================ -->
  <section>
    <h2>Toast (reuses Pill shell — Petal-Build-Map.md §2.2)</h2>
    <div class="row">
      <div class="cell">
        <Toast variant="reconnected" message="Switched to Ethernet" />
        <span class="caption">Reconnection — the exact DESIGN.md §9 example</span>
      </div>
      <div class="cell">
        <Toast variant="degraded" message="Connection is unstable" />
        <span class="caption">Degraded connection — amber, reuses --warning</span>
      </div>
      <div class="cell">
        <Toast variant="info" message="Recording saved to Downloads" dismissible />
        <span class="caption">Generic dismissible info toast</span>
      </div>
      <div class="cell">
        <Toast
          variant="info"
          message="Update ready — restart to apply"
          dismissible
          actionLabel="Restart now"
          onAction={() => {}}
        />
        <span class="caption">Update pending-relaunch — one-click "Restart now" action</span>
      </div>
    </div>
  </section>

  <!-- ============================================================ -->
  <section>
    <h2>Standard empty / offline states</h2>
    <div class="row">
      <div class="cell">
        <div class="frame">
          <EmptyState title="No rooms yet" detail="Create a room to get your team together." actionLabel="+ New room" />
        </div>
        <span class="caption">Generic empty state — quiet text + optional action, no illustration</span>
      </div>
      <div class="cell">
        <div class="frame">
          <OfflineState />
        </div>
        <span class="caption">Offline / connection-lost state — default copy</span>
      </div>
      <div class="cell">
        <div class="frame">
          <OfflineState
            title="Can't reach eng-sync"
            detail="Check your network connection and try again."
            onRetry={() => console.log('retry')}
          />
        </div>
        <span class="caption">Offline state — with explicit retry action</span>
      </div>
    </div>
  </section>
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
    max-width: 640px;
  }

  section {
    margin-bottom: 48px;
  }

  h2 {
    font-size: 14px;
    font-weight: 600;
    color: var(--text-muted);
    margin: 0 0 16px;
    padding-bottom: 8px;
    border-bottom: 1px solid var(--hairline-strong);
  }

  .row {
    display: flex;
    flex-wrap: wrap;
    gap: 32px;
    align-items: flex-start;
  }

  .cell {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 8px;
  }

  .caption {
    font-size: 10.5px;
    font-family: var(--font-mono);
    color: var(--text-muted);
    text-align: center;
    max-width: 220px;
  }

  .frame {
    width: 260px;
    border-radius: var(--radius-card);
    border: 1px dashed var(--hairline-strong);
    background: var(--surface);
  }
</style>
