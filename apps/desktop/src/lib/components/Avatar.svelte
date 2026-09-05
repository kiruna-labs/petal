<!--
  Avatar — circular participant avatar. Image if provided, else an initials
  fallback; optional identity-color ring + tinted initials, since avatars
  carry identity color at the call site (main menu, participant tiles,
  telepointers) even though tiles themselves stay graphite/monochrome per
  the design reversal (Petal-Build-Map.md §3, §2.6). Reference treatment:
  the compact Pill avatar (canvas.html §6 Pill UI) — a plum
  ring + a plum-tinted gradient fill behind plum-light initials.

  Identity palette (tokens.css --id-*): plum, blue, green, amber, lilac, slate.
-->
<script lang="ts" module>
  export type IdentityColor = 'plum' | 'blue' | 'green' | 'amber' | 'lilac' | 'slate';
</script>

<script lang="ts">
  interface Props {
    name: string;
    src?: string;
    size?: number;
    /** Identity-palette ring + tint. Omit for the default graphite look. */
    identity?: IdentityColor;
    /** Concrete meeting-scoped collision-resolved color. */
    resolvedColor?: string;
    /** Show the quiet speaking ring (used on gallery tiles, not avatars generally). */
    speaking?: boolean;
    /** Opt-in empty state for setup flows; regular avatars keep initials fallback behavior. */
    emptyPlaceholder?: 'person';
  }

  let {
    name,
    src,
    size = 32,
    identity,
    resolvedColor,
    speaking = false,
    emptyPlaceholder
  }: Props = $props();

  const initials = $derived(
    Array.from(name.trim())[0]?.toLocaleUpperCase() ?? '?'
  );
  const showEmptyPlaceholder = $derived(!src && !name.trim() && emptyPlaceholder === 'person');

  const identityVar = $derived(resolvedColor ?? (identity ? `var(--id-${identity})` : null));

  // A broken src must fall back to the initials chip, not a browser
  // broken-image glyph. Reset whenever the src changes.
  let imgFailed = $state(false);
  $effect(() => {
    imgFailed = false;
    void src;
  });
</script>

<div
  class="avatar"
  class:has-ring={Boolean(identityVar)}
  class:speaking
  style:width="{size}px"
  style:height="{size}px"
  style:--ring-color={identityVar}
  style:font-size="{Math.max(10, Math.round(size * 0.36))}px"
>
  {#if identityVar}
    <div class="ring" aria-hidden="true"></div>
  {/if}
  {#if src && !imgFailed}
    <img {src} alt={name} class="avatar-img" onerror={() => (imgFailed = true)} />
  {:else}
    <div
      class="avatar-fallback"
      class:tinted={Boolean(identityVar)}
    >
      {#if showEmptyPlaceholder}
        <span class="person-placeholder" aria-hidden="true"></span>
      {:else}
        <span>{initials}</span>
      {/if}
    </div>
  {/if}
</div>

<style>
  .avatar {
    position: relative;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    border-radius: var(--radius-pill);
    flex-shrink: 0;
    font-family: var(--font-ui);
    font-weight: 600;
  }

  .ring {
    position: absolute;
    inset: -2px;
    border-radius: var(--radius-pill);
    border: 1.5px solid var(--ring-color);
    pointer-events: none;
  }

  /* Quiet speaking ring — thin, dim, minimal halo per Petal-Build-Map.md §2.4. */
  .avatar.speaking::after {
    content: '';
    position: absolute;
    inset: -2px;
    border-radius: var(--radius-pill);
    border: 1.5px solid rgba(255, 255, 255, 0.55);
    box-shadow: 0 0 14px -6px rgba(255, 255, 255, 0.22);
    pointer-events: none;
  }

  .avatar-img {
    width: 100%;
    height: 100%;
    border-radius: var(--radius-pill);
    object-fit: cover;
    outline: 1px solid var(--hairline-strong);
    outline-offset: -1px;
    display: block;
  }

  .avatar-fallback {
    width: 100%;
    height: 100%;
    border-radius: var(--radius-pill);
    display: flex;
    align-items: center;
    justify-content: center;
    background: var(--surface-2);
    color: var(--text-primary);
    overflow: hidden;
  }

  /* Identity-tinted fallback — mirrors the plum "CR" avatar reference in
     canvas.html (dark tint gradient behind a light-tinted initial). */
  .avatar-fallback.tinted {
    background: linear-gradient(160deg, var(--surface-2), var(--bg-base-2));
    color: var(--ring-color);
  }

  .avatar-fallback span {
    line-height: 1;
  }

  .person-placeholder {
    position: relative;
    width: 46%;
    height: 46%;
    opacity: 0.62;
  }

  .person-placeholder::before,
  .person-placeholder::after {
    content: '';
    position: absolute;
    left: 50%;
    transform: translateX(-50%);
    background: currentColor;
  }

  .person-placeholder::before {
    top: 0;
    width: 42%;
    height: 42%;
    border-radius: var(--radius-pill);
  }

  .person-placeholder::after {
    bottom: 0;
    width: 78%;
    height: 42%;
    border-radius: 999px 999px 42% 42%;
  }
</style>
