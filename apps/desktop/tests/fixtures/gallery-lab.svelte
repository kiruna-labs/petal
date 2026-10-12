<!-- Holds the lab scenario as state so the matrix can re-render one mounted
     window (window.__galleryLab.set) instead of reloading per cell. -->
<script>
  import GalleryLabStage from '$lib/dev/GalleryLabStage.svelte';

  let { initial, onSettled } = $props();
  let scenario = $state(initial);
  let generation = $state(0);

  export function set(next) {
    scenario = next;
    generation += 1;
  }
</script>

<GalleryLabStage {scenario} onSettled={() => onSettled?.(generation)} />
