<!--
  Wordmark — the lowercase "petal" brand treatment (Fredoka 600), extracted
  from the Phase 0 placeholder in +page.svelte so it's reusable (main menu
  hero, launch screen, etc. per Petal-Build-Map.md §2.5 / §0).
-->
<script lang="ts">
  interface Props {
    /** Font size in px. Defaults to the Phase 0 placeholder hero size. */
    size?: number;
  }

  let { size = 56 }: Props = $props();
</script>

<span class="wordmark" style:font-size="{size}px">petal</span>

<style>
  .wordmark {
    display: inline-block;
    margin: 0;
    font-family: 'Fredoka', var(--font-display);
    font-weight: 600;
    letter-spacing: 0;
    color: var(--text-primary);
    -webkit-font-smoothing: antialiased;
  }
</style>
