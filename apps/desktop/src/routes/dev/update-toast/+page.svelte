<script lang="ts">
  import { onMount } from 'svelte';
  import ToastHost from '$lib/components/ToastHost.svelte';
  import { markUpdateAvailable } from '$lib/stores/updateStatus.svelte';

  onMount(() => {
    markUpdateAvailable('2.0.0-beta.20260712.123456');
  });
</script>

<ToastHost />
