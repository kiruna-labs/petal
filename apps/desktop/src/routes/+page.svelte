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

  Route decision (`$lib/data/launchLocation`'s `launchRoute`, in order):
  - running from the disk image / App Translocation / read-only → /relocate
    (#172) — before everything else, so returning users see it too;
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
  import { fetchLaunchLocationClass, launchRoute } from '$lib/data/launchLocation';
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
      const hasBridge = hasTauriBridge();
      // #172: a disk-image or App-Translocated run is diverted to /relocate
      // before anything else -- `launchRoute` puts the location first so a
      // returning, fully-permissioned user still sees it. A fast local read;
      // any failure reads as `other` and never blocks the launch.
      const location = hasBridge ? await fetchLaunchLocationClass() : null;
      const onboardingComplete = session.onboardingComplete;

      const early = launchRoute({ onboardingComplete, hasBridge, location, permissionsOk: true });
      if (early === '/relocate' || !onboardingComplete || !hasBridge) {
        // Browser fallback (no bridge): localStorage-only decision (see
        // header comment).
        goto(early, { replaceState: true });
        return;
      }

      // Warm-up is background; the permission recheck is a fast LOCAL
      // TCC/IPC read (no network), so it gates the /main paint — a
      // revoked-permission user must not flash the menu for the invoke
      // duration before being bounced to onboarding. listRooms() (the
      // network part) stays background per #8.
      listRooms().catch((e) => console.warn('launch: listRooms warm-up failed', e));
      const route = launchRoute({
        onboardingComplete,
        hasBridge,
        location,
        permissionsOk: await permissionsOk()
      });
      goto(route, { replaceState: true });
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
