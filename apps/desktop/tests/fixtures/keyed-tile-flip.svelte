<script lang="ts">
  import { flushSync } from 'svelte';
  import { uniformTileFlip } from '$lib/motion';

  // A keyed list driven by the real `uniformTileFlip`, as Gallery.svelte's is.
  // Tile "a" stays first while people join, and every count gives it a new
  // shape, so each join is a shape-changing (clipped) FLIP. Long enough that
  // a second join always lands mid-flight.
  const SIZES: Record<number, [number, number]> = { 2: [400, 225], 3: [240, 240], 4: [300, 150] };
  let ids = $state(['a', 'b']);
  const size = $derived(SIZES[ids.length] ?? [200, 200]);

  (window as unknown as { __keyedTileFlip: { join: () => void } }).__keyedTileFlip = {
    join: () => flushSync(() => (ids = [...ids, String.fromCharCode(97 + ids.length)]))
  };
</script>

<div class="list">
  {#each ids as id (id)}
    <div
      class="tile"
      data-id={id}
      style:width="{size[0]}px"
      style:height="{size[1]}px"
      animate:uniformTileFlip={{ duration: 8000 }}
    ></div>
  {/each}
</div>

<style>
  .list {
    display: flex;
    flex-wrap: wrap;
    gap: 12px;
    padding: 24px;
  }
  .tile {
    flex: none;
    border-radius: 12px;
    background: #3a6ea5;
  }
</style>
