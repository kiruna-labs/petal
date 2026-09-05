<!--
  Dev-only visual QA harness for the menubar pill + its popover (see
  src-tauri/src/menubar.rs). Matches the /dev/* pattern used by every prior
  phase (dev/components, dev/settings, etc.) -- throwaway scaffolding, fine
  to leave under /dev/.

  Two things to sanity-check here, since the real NSStatusItem itself is
  drawn natively (Core Graphics into an NSImage, not a webview) and can't be
  rendered in a browser preview:

  1. An HTML/CSS APPROXIMATION of the native pill's three states -- full
     in-call pill (canvas.html §3: live-green, dark-ink mic, people+count,
     dark leave circle with red glyph), minimal squeezed mode (light glyph +
     green dot), and the not-in-meeting neutral glyph (judgment call, see
     issue #4 Notes) -- so the layout/color/content decisions can be
     eyeballed against menubar.rs's paint() code. This is NOT a live mirror
     of the native rendering -- it's a stand-in built from the same comp
     values, flagged clearly as an approximation, not the source of truth.
  2. The popover composition (RosterPopover + feature-labeled control row,
     issue #6 labels: Audio / Video / Leave). The REAL route
     (src/routes/menubar-popover/+page.svelte) is presence-fed with an
     empty state; this harness shows the same composition with fixture data.
-->
<script lang="ts">
  import ControlButton from '$lib/components/ControlButton.svelte';
  import RosterPopover from '$lib/components/RosterPopover.svelte';
  import type { RosterParticipant } from '$lib/components/RosterPopover.svelte';

  let micMuted = $state(false);
  let participantCount = $state(4);

  const participants: RosterParticipant[] = [
    { name: 'Jordan Kim', identity: 'plum', isYou: true },
    { name: 'Marco Diaz', identity: 'blue', speaking: true },
    { name: 'Devin Osei', identity: 'green' },
    { name: 'Sana Patel', identity: 'amber', muted: true }
  ];
</script>

<div class="harness">
  <h1>Petal — menubar pill + popover dev harness</h1>
  <p class="intro">
    The native <code>NSStatusItem</code> pill itself is drawn with Core Graphics in
    <code>src-tauri/src/menubar.rs</code> — it cannot render inside a browser preview. The
    swatches below are an HTML/CSS <strong>approximation</strong> of the approved comp
    (canvas.html §3) for layout/color QA only, not the source of truth. The popover below it
    mirrors the composition of <code>src/routes/menubar-popover/+page.svelte</code> (the real
    route is presence-fed with a "Not in a meeting" empty state).
  </p>

  <section>
    <h2>Pill approximation (in-call full · in-call minimal · not in meeting)</h2>
    <p class="note">
      Full pill = live-green background, dark-ink mic (click zone: mute), people icon +
      real presence count, and a separate dark circle with the red leave glyph (its own click
      zone). Minimal mode is the squeeze fallback (menu bar width budget — see
      <code>menubar.rs</code>'s <code>effective_mode</code>): light glyph + green live dot.
      Not-in-meeting shows a dimmer neutral glyph, no green anywhere (nothing is live).
    </p>
    <div class="row">
      <div class="cell">
        <div class="menubar-swatch">
          <div class="pill-full">
            <span class="glyph ink" class:muted-slash={micMuted}>
              <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.8" stroke-linecap="round">
                <rect x="9" y="3" width="6" height="11" rx="3"></rect>
                <path d="M5 11a7 7 0 0 0 14 0M12 18v3" />
                {#if micMuted}<path d="M3 3l21 21" />{/if}
              </svg>
            </span>
            <span class="people-group">
              <svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.6" stroke-linecap="round">
                <circle cx="9" cy="8" r="3.5"></circle>
                <path d="M3 20a6 6 0 0 1 12 0"></path>
                <path d="M16 5.5a3.5 3.5 0 0 1 0 7"></path>
                <path d="M19 20a6 6 0 0 0-4-5.6"></path>
              </svg>
              <span class="count">{Math.min(participantCount, 99)}</span>
            </span>
            <span class="leave-circle" title="Leave (own click zone)">
              <svg width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.8" stroke-linecap="round" stroke-linejoin="round">
                <path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4"></path>
                <path d="M16 17l5-5-5-5M21 12H9"></path>
              </svg>
            </span>
          </div>
        </div>
        <span class="caption">Full — green pill, mic, people + count, leave circle</span>
      </div>

      <div class="cell">
        <div class="menubar-swatch">
          <div class="pill-minimal">
            <span class="glyph light">
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.3" stroke-linecap="round">
                <rect x="9" y="3" width="6" height="11" rx="3"></rect>
                <path d="M5 11a7 7 0 0 0 14 0M12 18v3" />
                {#if micMuted}<path d="M3 3l21 21" />{/if}
              </svg>
              <span class="live-dot"></span>
            </span>
          </div>
        </div>
        <span class="caption">Minimal (squeezed) — glyph + live dot</span>
      </div>

      <div class="cell">
        <div class="menubar-swatch">
          <div class="pill-minimal">
            <span class="glyph idle">
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.3" stroke-linecap="round">
                <rect x="9" y="3" width="6" height="11" rx="3"></rect>
                <path d="M5 11a7 7 0 0 0 14 0M12 18v3" />
              </svg>
            </span>
          </div>
        </div>
        <span class="caption">Not in a meeting — neutral, no dot</span>
      </div>
    </div>

    <div class="controls">
      <label>
        <input type="checkbox" bind:checked={micMuted} />
        mic muted
      </label>
      <label>
        participant count
        <input type="number" min="0" max="99" bind:value={participantCount} style="width: 48px" />
      </label>
    </div>
  </section>

  <section>
    <h2>Popover (composition mirror)</h2>
    <p class="note">
      Reuses <code>RosterPopover</code> + <code>ControlButton</code> as-is — the popover itself
      is explicitly flagged as not-yet-designed, so no new visual design was invented here.
      Controls use icon-only buttons with delayed tooltips; state lives in the button treatment.
    </p>
    <div class="row">
      <div class="cell">
        <div class="popover-frame">
          <RosterPopover roomName="eng-sync" {participants} embedded />
          <div class="control-row">
            <div class="control">
              <ControlButton icon="mic" kind="toggle" active={micMuted} size="compact" label={micMuted ? 'Unmute microphone' : 'Mute microphone'} onclick={() => (micMuted = !micMuted)} />
              <span class="control-tooltip" aria-hidden="true">Audio</span>
            </div>
            <div class="control">
              <ControlButton icon="camera" kind="toggle" size="compact" label="Turn camera on" />
              <span class="control-tooltip" aria-hidden="true">Video</span>
            </div>
            <div class="spacer"></div>
            <div class="control">
              <ControlButton icon="leave" kind="oneshot" tone="danger" size="compact" label="Leave meeting" />
              <span class="control-tooltip leave-tooltip" aria-hidden="true">Leave</span>
            </div>
          </div>
        </div>
        <span class="caption">menubar-popover route composition (fixture data)</span>
      </div>
    </div>
  </section>
</div>

<style>
  .harness {
    padding: 32px;
    background: var(--bg-base, #0a0a0b);
    min-height: 100vh;
    font-family: var(--font-ui, -apple-system, system-ui, sans-serif);
    color: var(--text-primary, #ededef);
  }

  h1 {
    font: 700 20px var(--font-display, sans-serif);
    margin: 0 0 4px;
  }

  h2 {
    font: 600 14px var(--font-ui);
    margin: 0 0 6px;
    color: var(--text-primary);
  }

  .intro,
  .note {
    color: var(--text-muted, #8fa6b8);
    font-size: 12.5px;
    max-width: 720px;
    margin: 0 0 18px;
  }

  section {
    margin-bottom: 40px;
  }

  .row {
    display: flex;
    gap: 28px;
    align-items: flex-start;
    flex-wrap: wrap;
  }

  .cell {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: 8px;
  }

  .caption {
    font-size: 11px;
    color: var(--text-faint, rgba(255, 255, 255, 0.45));
  }

  .controls {
    display: flex;
    gap: 20px;
    margin-top: 14px;
    font-size: 12.5px;
    color: var(--text-muted);
    align-items: center;
  }

  /* Menu-bar backdrop swatch so the pill approximation reads in context
     (canvas.html §3's dark menu-bar strip). */
  .menubar-swatch {
    display: inline-flex;
    align-items: center;
    height: 32px;
    padding: 0 12px;
    border-radius: 8px;
    background: rgba(22, 22, 24, 0.92);
    box-shadow: inset 0 0 0 1px rgba(255, 255, 255, 0.06);
  }

  /* Full in-call pill -- canvas.html §3 values (#34C759 bg, #062B12 ink,
     #0a0a0c leave circle, #FF6B5E leave glyph), 22pt tall like paint(). */
  .pill-full {
    display: inline-flex;
    align-items: center;
    gap: 7px;
    height: 22px;
    padding: 0 2px 0 9px;
    border-radius: 999px;
    background: #34c759;
  }

  .glyph {
    display: inline-flex;
    position: relative;
  }

  .glyph.ink {
    color: #062b12;
  }

  .glyph.light {
    color: rgba(255, 255, 255, 0.85);
  }

  .glyph.idle {
    color: rgba(255, 255, 255, 0.62);
  }

  .people-group {
    display: inline-flex;
    align-items: center;
    gap: 3px;
    color: #062b12;
  }

  .count {
    font: 700 10px var(--font-mono, monospace);
    color: #062b12;
  }

  .leave-circle {
    width: 18px;
    height: 18px;
    border-radius: 999px;
    background: #0a0a0c;
    color: #ff6b5e;
    display: inline-flex;
    align-items: center;
    justify-content: center;
  }

  /* Minimal / not-in-meeting: no pill background at all. */
  .pill-minimal {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 34px;
    height: 22px;
  }

  .live-dot {
    position: absolute;
    right: -1px;
    bottom: -1px;
    width: 6px;
    height: 6px;
    border-radius: 999px;
    background: #34c759;
  }

  .popover-frame {
    display: flex;
    flex-direction: column;
    width: 280px;
    border-radius: var(--radius-card);
    background: linear-gradient(180deg, var(--surface-raised), var(--surface));
    border: 1px solid var(--hairline);
    box-shadow: var(--shadow-panel, 0 30px 80px -28px rgba(0, 0, 0, 0.86));
    overflow: hidden;
  }

  .control-row {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 10px 14px;
    background: var(--surface, #16171a);
    border-top: 1px solid var(--hairline, rgba(255, 255, 255, 0.06));
    border-radius: 0 0 var(--radius-card, 16px) var(--radius-card, 16px);
  }

  .control {
    position: relative;
    display: flex;
    align-items: center;
    justify-content: center;
  }

  .control-tooltip {
    position: absolute;
    left: 50%;
    bottom: calc(100% + 8px);
    z-index: 4;
    padding: 5px 8px;
    border: 1px solid rgba(255, 255, 255, 0.1);
    border-radius: var(--radius-chip, 999px);
    background: rgba(20, 22, 24, 0.94);
    box-shadow: 0 10px 28px rgba(0, 0, 0, 0.28);
    color: var(--text-muted, rgba(255, 255, 255, 0.72));
    font: 600 11px var(--font-ui, sans-serif);
    line-height: 1.2;
    white-space: nowrap;
    opacity: 0;
    pointer-events: none;
    transform: translate(-50%, 4px);
    transition:
      opacity var(--motion-fast, 120ms) var(--ease-standard, ease),
      transform var(--motion-fast, 120ms) var(--ease-standard, ease);
  }

  .leave-tooltip {
    color: var(--danger, #ff6b5e);
  }

  .control:hover .control-tooltip,
  .control:has(:focus-visible) .control-tooltip {
    opacity: 1;
    transform: translate(-50%, 0);
    transition-delay: 550ms;
  }

  .spacer {
    flex: 1;
  }
</style>
