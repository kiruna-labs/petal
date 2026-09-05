<script lang="ts">
  import { onMount } from 'svelte';

  let phase = $state('prepare');
  let detail = $state('');
  let remainingSeconds = $state(0);
  let countdownHandle: ReturnType<typeof setInterval> | null = null;

  function updateFromNativeDeadline(deadlineEpochMs: number) {
    remainingSeconds = Math.max(0, Math.ceil((deadlineEpochMs - Date.now()) / 1000));
  }

  onMount(() => {
    const params = new URLSearchParams(window.location.search);
    phase = params.get('cockpitPhase') ?? 'prepare';
    detail = params.get('detail') ?? '';
    const deadlineEpochMs = Number(params.get('deadlineEpochMs') ?? '0');
    if (phase === 'prepare' && Number.isFinite(deadlineEpochMs) && deadlineEpochMs > 0) {
      updateFromNativeDeadline(deadlineEpochMs);
      countdownHandle = setInterval(() => updateFromNativeDeadline(deadlineEpochMs), 250);
    }
    return () => {
      if (countdownHandle !== null) clearInterval(countdownHandle);
    };
  });

  const message = $derived(
    phase === 'prepare'
      ? `Automation begins in ${remainingSeconds}s. Keep the test-pattern window frontmost.`
      : phase === 'starting'
        ? 'Capture setup is starting. Keep the test-pattern window frontmost.'
        : phase === 'capture-locked'
          ? 'Capture locked. Capture is active; keep the test-pattern window frontmost.'
          : `Cockpit capture did not start${detail ? `: ${detail}` : ''}.`
  );
</script>

<svelte:head><title>Petal Test Cockpit status</title></svelte:head>

<main class:failed={phase === 'failed'} class:locked={phase === 'capture-locked'}>
  <section role="status" aria-live="polite" aria-atomic="true" aria-label="Cockpit capture status">
    <strong>{phase === 'prepare' ? `Prepare ${remainingSeconds}s` : phase.replace('-', ' ')}</strong>
    <span>{message}</span>
  </section>
</main>

<style>
  :global(html), :global(body) { margin: 0; width: 100%; height: 100%; overflow: hidden; }
  main { height: 100%; display: grid; place-items: center; background: #573400; color: #fff; font-family: system-ui, sans-serif; }
  main.locked { background: #073c30; }
  main.failed { background: #651b1b; }
  section { display: flex; align-items: baseline; justify-content: center; gap: 16px; padding: 12px 20px; text-align: center; }
  strong { font-size: 20px; text-transform: capitalize; }
  span { font-size: 16px; }
</style>
