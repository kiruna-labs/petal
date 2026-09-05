<!--
  NamePill — the rounded name label attached to a telepointer, in the
  pointer-owner's identity color. Per Petal-Build-Map.md §2.6 / §3: this is
  one of the two places identity color survives the monochrome reversal
  (the other being Pointer.svelte itself, and per the project README's
  assumed default, RemoteWindowHeader's accent stripe).

  Colors are pulled from tokens.css's --id-* custom properties (never
  hardcoded hex) exactly as Avatar.svelte already does for its identity
  ring, so the identity→color mapping lives in exactly one place.

  Text-on-fill color: the reference pill uses bold dark ink on a bright fill.
  We keep the functional identity fill, then derive a subtle per-color dark ink
  with color-mix instead of collapsing all users to one cyan.
-->
<script lang="ts">
  import type { IdentityColor } from './Avatar.svelte';

  interface Props {
    name: string;
    identity: IdentityColor;
    /** Idle telepointers fade their label per Build-Map §2.6 states. */
    idle?: boolean;
  }

  let { name, identity, idle = false }: Props = $props();
</script>

<span class="name-pill" class:idle style:--id-color="var(--id-{identity})">
  {name}
</span>

<style>
  .name-pill {
    display: inline-flex;
    align-items: center;
    font: 800 10.5px/1 var(--font-ui);
    letter-spacing: 0.045em;
    /* Preserve the name exactly as provided — do NOT uppercase (#telepointer
       name casing bug). The approved mock's telepointer board renders
       "Priya"/"Chantelle" in mixed case, and
       web-harness's equivalent .remote-telepointer__label already sets
       text-transform: none for the same reason. */
    text-transform: none;
    color: rgba(10, 10, 12, 0.88);
    color: color-mix(in srgb, var(--id-color) 16%, var(--mix-base));
    background: var(--id-color);
    background: linear-gradient(180deg, color-mix(in srgb, var(--id-color) 92%, var(--text-primary)), var(--id-color));
    padding: 5px 9px 4px;
    border-radius: var(--radius-pill);
    max-width: min(240px, calc(100vw - 16px));
    overflow: visible;
    overflow-wrap: anywhere;
    text-wrap: pretty;
    white-space: normal;
    transition: opacity var(--motion-base) var(--ease-standard);
  }

  /* Idle: label fades — actual idle-timeout detection is a later wiring
     concern (Build-Map §2.6); this component only reacts to the prop. */
  .name-pill.idle {
    opacity: 0.45;
  }
</style>
