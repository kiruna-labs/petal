<!--
  Pill — the reusable rounded compact container. Per Petal-Build-Map.md §2.2 /
  DESIGN.md §6 "Pill UI": one true-black capsule shell (999px radius, subtle
  vibrancy-style gradient + hairline + shadow) reused for two different
  contents — the in-meeting compact/small state (avatar + count + controls)
  and a status toast ("Switched to Ethernet"). Generic container with a
  default slot; callers supply the contents.
-->
<script lang="ts">
  import type { Snippet } from 'svelte';

  interface Props {
    /** Toast-style pills read a bit more spacious/padded than the control pill. */
    padded?: boolean;
    /** Let transient text surfaces grow when copy wraps across lines. */
    autoHeight?: boolean;
    /** Vertical capsule variant (issue #12): same shell, contents
     * stacked top-to-bottom — used by the floating meeting pill when it
     * hugs a left/right screen edge. */
    orientation?: 'horizontal' | 'vertical';
    /** Larger collapsed meeting pill. Kept as real layout metrics rather
     * than transform scaling so native window measurement follows it. */
    scale?: 'normal' | 'large';
    /** Right-edge hover-tab shell. The native tab grows inward from the
     * window's right-center edge; omitting the prop keeps the classic capsule
     * contract used by meeting chrome and toasts. */
    attach?: 'right';
    children?: Snippet;
  }

  let {
    padded = false,
    autoHeight = false,
    orientation = 'horizontal',
    scale = 'normal',
    attach = undefined,
    children
  }: Props = $props();
</script>

<div
  class="pill"
  class:padded
  class:auto-height={autoHeight}
  class:vertical={orientation === 'vertical'}
  class:large={scale === 'large'}
  class:attach={attach !== undefined}
  class:attach-right={attach === 'right'}
>
  {@render children?.()}
</div>

<style>
  .pill {
    display: inline-flex;
    align-items: center;
    gap: 9px;
    height: 46px;
    padding: 0 12px 0 9px;
    border-radius: var(--radius-pill);
    background: linear-gradient(180deg, var(--surface), var(--bg-base));
    border: none;
    box-shadow: var(--shadow-pill);
    color: var(--text-primary);
    box-sizing: border-box;
  }

  /* Vertical capsule (issue #12): the horizontal metrics rotated 90° —
     46px wide, padding 9px top / 12px bottom (mirrors the 9px-left/12px-right
     horizontal asymmetry), contents stacked. */
  .pill.vertical {
    flex-direction: column;
    height: auto;
    width: 46px;
    padding: 9px 0 12px;
  }

  .pill.large {
    gap: 11.25px;
    height: 58px;
    padding: 0 15px;
  }

  .pill.auto-height {
    height: auto;
    min-height: 46px;
  }

  .pill.large.vertical {
    width: 58px;
    height: auto;
    padding: 11.25px 0 15px;
  }

  /* Toast-shaped content (e.g. icon + label, no circular controls) reads
     better with symmetric padding. */
  .pill.padded {
    gap: 10px;
    padding: 0 16px;
  }

  .pill.padded.auto-height {
    padding-top: 9px;
    padding-bottom: 9px;
  }

  /* Right-edge hover-tab shell: dark surface, compact geometry, and a
     visible ring. The hover route supplies the fixed trigger/tray layout.
     Keep these colors literal for the existing contrast/ui-consistency gate. */
  .pill.attach {
    height: 100%;
    width: 100%;
    padding: 4px 6px;
    gap: 6px;
    background: linear-gradient(180deg, #161618, #060607);
    border-radius: 12px 12px 0 0;
    box-shadow:
      inset 0 0 0 1px rgba(255, 255, 255, 0.2),
      inset 1px 0 0 rgba(255, 255, 255, 0.09),
      inset -1px 0 0 rgba(255, 255, 255, 0.09),
      inset 0 1px 0 rgba(255, 255, 255, 0.07),
      0 5px 16px rgba(0, 0, 0, 0.3);
  }

  .pill.attach-right {
    border-radius: 12px 0 0 12px;
    box-shadow:
      inset 0 0 0 1px rgba(255, 255, 255, 0.2),
      inset 1px 0 0 rgba(255, 255, 255, 0.09),
      inset -1px 0 0 rgba(255, 255, 255, 0.09),
      inset -1px 0 0 rgba(255, 255, 255, 0.07),
      -5px 0 16px rgba(0, 0, 0, 0.3);
  }
</style>
