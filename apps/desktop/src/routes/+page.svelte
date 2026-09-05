<!--
  `/` — launch router. Paints only the app background during the first frame,
  then immediately hands off to `/main` or `/onboarding`. There is no splash
  UI here (#639): the reveal gate (#636) already keeps the native window
  hidden until real first paint, so this route is only ever visible for a
  frame or two while `decide()` below runs.

  Real startup checks still run:
  - `check_screen_recording` + `check_microphone` + `check_accessibility`
    (src-tauri/src/permissions.rs via $lib/data/permissions.ts) — permission
    state is re-evaluated EVERY launch, replacing the old localStorage-only
    gate (which never re-onboarded a user who revoked a required permission
    after first run);
  - a best-effort `listRooms()` warm-up so `/main` paints with data.

  Route decision:
  - onboarding never completed → /onboarding immediately;
  - onboarding completed → /main immediately, then permission checks redirect
    to /onboarding only if a REQUIRED permission is missing.
  Navigation uses `replaceState` so Back never returns here and re-runs
  checks pointlessly.

  Graceful browser fallback (no `__TAURI_INTERNALS__` bridge): the permission
  wrappers would all report "missing" and force /onboarding forever, so with
  no bridge the decision falls back to the localStorage `onboardingComplete`
  flag alone — the real permission gate only means anything inside the real
  app.

  If `decide()` throws, it's logged and swallowed rather than shown here:
  by the time any awaited check could reject, this route has already
  `goto`'d away in every real path (onboarding-incomplete and no-bridge both
  `return` before the first `await`; the bridge path gates the `/main` paint
  on the fast local permission recheck and only then `goto`s), so there is no
  reachable moment where a user would actually be looking at this route to
  see an inline error for.
-->
<script lang="ts">
  import { goto } from '$app/navigation';
  import { onMount } from 'svelte';
  import { session } from '$lib/stores/session.svelte';
  import {
    checkScreenRecording,
    checkMicrophone,
    checkAccessibility
  } from '$lib/data/permissions';
  import { listRooms } from '$lib/data/rooms';
  import { hasTauriBridge } from '$lib/ipc';

  async function permissionsOk() {
    const [screen, mic, accessibility] = await Promise.all([
      checkScreenRecording(),
      checkMicrophone(),
      checkAccessibility()
    ]);
    return screen && mic === 'authorized' && accessibility;
  }

  async function decide() {
    try {
      if (!session.onboardingComplete) {
        goto('/onboarding', { replaceState: true });
        return;
      }

      if (!hasTauriBridge()) {
        // Browser fallback: localStorage-only decision (see header comment).
        goto('/main', { replaceState: true });
        return;
      }

      // Warm-up is background; the permission recheck is a fast LOCAL
      // TCC/IPC read (no network), so it gates the /main paint — a
      // revoked-permission user must not flash the menu for the invoke
      // duration before being bounced to onboarding. listRooms() (the
      // network part) stays background per #8.
      listRooms().catch((e) => console.warn('launch: listRooms warm-up failed', e));
      if (!(await permissionsOk())) {
        goto('/onboarding', { replaceState: true });
        return;
      }
      goto('/main', { replaceState: true });
    } catch (e) {
      console.error('launch: startup checks failed', e);
    }
  }

  onMount(() => {
    decide();
  });
</script>

<main></main>

<style>
  main {
    height: 100%;
    width: 100%;
    background: var(--menu-shell);
  }
</style>
