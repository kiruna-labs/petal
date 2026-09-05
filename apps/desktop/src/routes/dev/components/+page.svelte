<!--
  Dev-only visual QA harness for the Phase 1 primitives (ControlButton, Pill,
  DensityToggle, Wordmark, Avatar). Renders every component in every state
  side by side, labeled with small captions, so states can be sanity-checked
  without a design tool. Throwaway scaffolding — fine to leave under /dev/.
-->
<script lang="ts">
  import ControlButton from '$lib/components/ControlButton.svelte';
  import Pill from '@petal/shared/ui/components/Pill.svelte';
  import DensityToggle from '$lib/components/DensityToggle.svelte';
  import Wordmark from '$lib/components/Wordmark.svelte';
  import Avatar from '$lib/components/Avatar.svelte';
  import ParticipantTile from '$lib/components/ParticipantTile.svelte';
  import Gallery from '$lib/components/Gallery.svelte';
  import type { GalleryParticipant } from '$lib/components/Gallery.svelte';
  import Pointer from '$lib/components/Pointer.svelte';
  import RemoteWindowHeader from '$lib/components/RemoteWindowHeader.svelte';
  import Filmstrip from '$lib/components/Filmstrip.svelte';
  import type { FilmstripParticipant } from '$lib/components/Filmstrip.svelte';
  import MeetingChrome from '$lib/components/MeetingChrome.svelte';
  import { identityColorCss, identityInkCss } from '$lib/data/identityColor';
  import Button from '$lib/components/Button.svelte';
  import Modal from '$lib/components/Modal.svelte';
  import FeedbackModal from '$lib/components/FeedbackModal.svelte';
  import Checkbox from '@petal/shared/ui/components/Checkbox.svelte';
  import LiveHero from '$lib/components/LiveHero.svelte';
  import RoomRow from '$lib/components/RoomRow.svelte';
  import MediaSplitControl from '$lib/components/MediaSplitControl.svelte';

  let micMuted = $state(false);
  let camActive = $state(false);
  let shareActive = $state(true);
  let density = $state<'comfortable' | 'compact'>('comfortable');

  // ---- Phase 2 demo state ----
  const GALLERY_NAMES = [
    'You', 'Chantelle', 'Priya', 'Marco', 'Devin', 'Sana',
    'Jordan', 'Alex', 'Riley', 'Sam', 'Avery', 'Morgan',
    'Taylor', 'Quinn', 'Drew', 'Blake', 'Casey', 'Jamie',
    'Robin', 'Sage'
  ];

  function makeGalleryParticipants(count: number): GalleryParticipant[] {
    return Array.from({ length: count }, (_, i) => {
      if (i === 0) return { name: 'You', videoOn: true, isLocal: true };
      if (i === 1) return {
        name: 'Chantelle', videoOn: true, speaking: true, sharing: true,
        shareCount: 3,
        sharingLiveBackground: identityColorCss('green'),
        sharingLiveColor: identityInkCss('green')
      };
      if (i === 2) return { name: 'Priya', videoOn: true, weakConnection: true };
      if (i === 3) return { name: 'Marco', videoOn: false, muted: true };
      if (i === 4) return { name: 'Devin', videoOn: false, muted: true };
      if (i === 5) return { name: 'Sana', videoOn: false };
      const name = GALLERY_NAMES[i % GALLERY_NAMES.length] + (i >= GALLERY_NAMES.length ? ` ${Math.floor(i / GALLERY_NAMES.length) + 1}` : '');
      return { name, videoOn: i % 3 !== 0, muted: i % 5 === 0 };
    });
  }

  let galleryCount = $state(6);
  const galleryParticipants = $derived(makeGalleryParticipants(galleryCount));

  let pointerIdle = $state(false);
  let pointerPulse = $state(0);

  // ---- RemoteWindowHeader demo state ----
  let headerModes = $state([
    { remoteControl: false, draw: false },
    { remoteControl: false, draw: false },
    { remoteControl: false, draw: false },
    { remoteControl: false, draw: false },
  ]);
  function toggleHeaderRemoteControl(i: number) {
    headerModes[i].remoteControl = !headerModes[i].remoteControl;
    if (headerModes[i].remoteControl) headerModes[i].draw = false;
  }
  function toggleHeaderDraw(i: number) {
    headerModes[i].draw = !headerModes[i].draw;
    if (headerModes[i].draw) headerModes[i].remoteControl = false;
  }

  // ---- M4 MeetingChrome demo state ----
  let chromeExpanded = $state(true);
  let chromeMic = $state(false);
  let chromeCam = $state(false);
  // issue #12: vertical pill variant (screen-edge orientation flip).
  let chromeOrientation = $state<'horizontal' | 'vertical'>('horizontal');

  // issue #9: fake camera stream (canvas.captureStream) so the pill's
  // circular self-view is previewable without a real getUserMedia prompt.
  // An animated canvas (moving gradient blob) so the <video> demonstrably
  // plays live frames, not a frozen first frame.
  let chromeFakeStream = $state<MediaStream | null>(null);
  let fakeRaf = 0;

  let openModal = $state<'compact' | 'comfortable' | 'wide' | null>(null);
  let feedbackModalOpen = $state(false);
  let checkboxChecked = $state(false);
  let mediaSplitMicActive = $state(false);
  let mediaSplitCamActive = $state(false);

  const heroParticipants = [
    { name: 'Jordan Kim', identity: 'plum' as const },
    { name: 'Chantelle', identity: 'blue' as const },
    { name: 'Marco', identity: 'amber' as const },
    { name: 'Priya', identity: 'green' as const },
    { name: 'Devin', identity: 'lilac' as const }
  ];

  const liveRoomParticipants = [
    { name: 'Jordan Kim', identity: 'plum' as const },
    { name: 'Chantelle', identity: 'blue' as const },
    { name: 'Marco', identity: 'amber' as const }
  ];

  const currentRoomParticipants = [
    { name: 'Jordan Kim', identity: 'plum' as const },
    { name: 'Priya', identity: 'green' as const }
  ];

  function stopFakeStream() {
    cancelAnimationFrame(fakeRaf);
    chromeFakeStream?.getTracks().forEach((t) => t.stop());
    chromeFakeStream = null;
    chromeCam = false;
  }

  function toggleFakeStream() {
    if (chromeFakeStream) {
      stopFakeStream();
      return;
    }
    const canvas = document.createElement('canvas');
    canvas.width = 160;
    canvas.height = 120;
    const ctx = canvas.getContext('2d')!;
    const draw = (t: number) => {
      ctx.fillStyle = '#1c2733';
      ctx.fillRect(0, 0, 160, 120);
      const x = 80 + Math.cos(t / 600) * 40;
      const g = ctx.createRadialGradient(x, 60, 4, x, 60, 55);
      g.addColorStop(0, '#8fd3a8');
      g.addColorStop(1, '#1c2733');
      ctx.fillStyle = g;
      ctx.fillRect(0, 0, 160, 120);
      // An off-center marker so the scaleX(-1) mirroring is visually provable.
      ctx.fillStyle = '#e5484d';
      ctx.fillRect(8, 8, 26, 26);
      fakeRaf = requestAnimationFrame(draw);
    };
    fakeRaf = requestAnimationFrame(draw);
    chromeFakeStream = canvas.captureStream(30);
    chromeCam = true;
  }

  $effect(() => stopFakeStream); // release on page teardown

  // ---- M1 filmstrip demo state ----
  const FILMSTRIP_NAMES = [
    'You', 'Chantelle', 'Priya', 'Marco', 'Devin',
    'Sana', 'Jordan', 'Alex', 'Riley', 'Sam',
    'Avery', 'Morgan', 'Taylor', 'Quinn', 'Drew'
  ];

  function makeFilmstripParticipants(count: number): FilmstripParticipant[] {
    return Array.from({ length: count }, (_, i) => {
      if (i === 0) return { name: 'You', videoOn: true };
      if (i === 1) return { name: 'Chantelle', videoOn: true, speaking: true };
      if (i === 2) return { name: 'Priya', videoOn: true, weakConnection: true };
      if (i === 3) return { name: 'Marco', videoOn: false, muted: true };
      if (i === 4) return { name: 'Devin', videoOn: false, muted: true };
      const name = FILMSTRIP_NAMES[i % FILMSTRIP_NAMES.length] + (i >= FILMSTRIP_NAMES.length ? ` ${Math.floor(i / FILMSTRIP_NAMES.length) + 1}` : '');
      return { name, videoOn: i % 2 === 0, muted: i % 4 === 0 };
    });
  }

  let filmstripCount = $state(5);
  const filmstripParticipants = $derived(makeFilmstripParticipants(filmstripCount));
</script>

<div class="harness">
  <h1>Petal — component dev harness</h1>
  <p class="intro">Phase 1 primitives, every state, side by side. Dev-only route.</p>

  <!-- ============================================================ -->
  <section>
    <h2>Wordmark</h2>
    <div class="row">
      <div class="cell">
        <Wordmark size={56} />
        <span class="caption">Wordmark / size 56 (hero)</span>
      </div>
      <div class="cell">
        <Wordmark size={28} />
        <span class="caption">Wordmark / size 28 (compact)</span>
      </div>
    </div>
  </section>

  <!-- ============================================================ -->
  <section>
    <h2>ControlButton — 44px matrix (default / hover / on-active / disabled)</h2>
    <div class="matrix">
      <span class="row-label">Microphone</span>
      <div class="cell"><ControlButton icon="mic" kind="toggle" /><span class="caption">default</span></div>
      <div class="cell hover-force"><ControlButton icon="mic" kind="toggle" /><span class="caption">hover</span></div>
      <div class="cell"><ControlButton icon="mic" kind="toggle" active /><span class="caption">on/active (muted, neutral slash)</span></div>
      <div class="cell"><ControlButton icon="mic" kind="toggle" disabled /><span class="caption">disabled</span></div>

      <span class="row-label">Webcam</span>
      <div class="cell"><ControlButton icon="camera" kind="toggle" /><span class="caption">default</span></div>
      <div class="cell hover-force"><ControlButton icon="camera" kind="toggle" /><span class="caption">hover</span></div>
      <div class="cell"><ControlButton icon="camera" kind="toggle" active /><span class="caption">on/active (camera off — slash glyph, stays neutral per Build-Map §2.1)</span></div>
      <div class="cell"><ControlButton icon="camera" kind="toggle" disabled /><span class="caption">disabled</span></div>

      <span class="row-label">Screensharing</span>
      <div class="cell"><ControlButton icon="screenshare" kind="toggle" /><span class="caption">default</span></div>
      <div class="cell hover-force"><ControlButton icon="screenshare" kind="toggle" /><span class="caption">hover</span></div>
      <div class="cell"><ControlButton icon="screenshare" kind="toggle" active /><span class="caption">on/active (sharing, live green)</span></div>
      <div class="cell"><ControlButton icon="screenshare" kind="toggle" disabled /><span class="caption">disabled</span></div>

      <span class="row-label">Invite (one-shot)</span>
      <div class="cell"><ControlButton icon="invite" kind="oneshot" /><span class="caption">default</span></div>
      <div class="cell hover-force"><ControlButton icon="invite" kind="oneshot" /><span class="caption">hover</span></div>
      <div class="cell"><span class="dash">—</span><span class="caption">no active state (one-shot)</span></div>
      <div class="cell"><ControlButton icon="invite" kind="oneshot" disabled /><span class="caption">disabled</span></div>

      <span class="row-label">Leave (one-shot, danger)</span>
      <div class="cell"><ControlButton icon="leave" kind="oneshot" tone="danger" /><span class="caption">default (always red)</span></div>
      <div class="cell hover-force"><ControlButton icon="leave" kind="oneshot" tone="danger" /><span class="caption">hover</span></div>
      <div class="cell"><span class="dash">—</span><span class="caption">no active state (one-shot)</span></div>
      <div class="cell"><span class="dash">—</span><span class="caption">no disabled state shown in comps</span></div>
    </div>
  </section>

  <!-- ============================================================ -->
  <section>
    <h2>ControlButton — sizes</h2>
    <div class="row">
      <div class="cell"><ControlButton icon="mic" size={44} /><span class="caption">size=44</span></div>
      <div class="cell"><ControlButton icon="mic" size="compact" /><span class="caption">size=compact (32px)</span></div>
      <div class="cell"><ControlButton icon="mic" size="menubar" /><span class="caption">size=menubar (24px)</span></div>
    </div>
  </section>

  <!-- ============================================================ -->
  <section>
    <h2>ControlButton — interactive toggle demo</h2>
    <div class="row">
      <div class="cell">
        <ControlButton
          icon="mic"
          kind="toggle"
          active={micMuted}
          onclick={() => (micMuted = !micMuted)}
        />
        <span class="caption">click to mute/unmute (slash draws on/off)</span>
      </div>
      <div class="cell">
        <ControlButton
          icon="camera"
          kind="toggle"
          active={camActive}
          onclick={() => (camActive = !camActive)}
        />
        <span class="caption">click to toggle camera off</span>
      </div>
      <div class="cell">
        <ControlButton
          icon="screenshare"
          kind="toggle"
          active={shareActive}
          onclick={() => (shareActive = !shareActive)}
        />
        <span class="caption">click to toggle sharing</span>
      </div>
    </div>
  </section>

  <!-- ============================================================ -->
  <section>
    <h2>Pill</h2>
    <div class="row">
      <div class="cell">
        <Pill>
          <Avatar name="Chantelle" identity="plum" size={32} />
          <span class="pill-count">+3</span>
          <ControlButton icon="mic" size="compact" />
          <ControlButton icon="leave" kind="oneshot" tone="danger" size="compact" />
        </Pill>
        <span class="caption">Pill / in-meeting compact state</span>
      </div>
      <div class="cell">
        <Pill padded>
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="var(--live-bright)" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <path d="M5 12.5 10 17.5 19 7"></path>
          </svg>
          <span class="toast-text">Switched to Ethernet</span>
        </Pill>
        <span class="caption">Pill / status toast shell</span>
      </div>
    </div>
  </section>

  <!-- ============================================================ -->
  <section>
    <h2>DensityToggle</h2>
    <div class="row">
      <div class="cell">
        <DensityToggle variant="segmented" bind:density />
        <span class="caption">segmented / selected: {density}</span>
      </div>
      <div class="cell">
        <DensityToggle variant="chevron" bind:density />
        <span class="caption">chevron / {density === 'comfortable' ? 'expanded (click to collapse)' : 'collapsed (click to expand)'}</span>
      </div>
    </div>
  </section>

  <!-- ============================================================ -->
  <section>
    <h2>Avatar</h2>
    <div class="row">
      <div class="cell">
        <Avatar name="Jordan Kim" size={44} />
        <span class="caption">initials fallback, no identity</span>
      </div>
      <div class="cell">
        <Avatar name="Chantelle Reyes" size={44} identity="plum" />
        <span class="caption">identity=plum ring + tint</span>
      </div>
      <div class="cell">
        <Avatar name="Marco" size={44} identity="blue" />
        <span class="caption">identity=blue ring + tint</span>
      </div>
      <div class="cell">
        <Avatar name="Sana" size={44} identity="green" />
        <span class="caption">identity=green ring + tint</span>
      </div>
      <div class="cell">
        <Avatar name="Priya" size={44} identity="amber" />
        <span class="caption">identity=amber ring + tint</span>
      </div>
      <div class="cell">
        <Avatar name="Devin" size={44} identity="lilac" />
        <span class="caption">identity=lilac ring + tint</span>
      </div>
      <div class="cell">
        <Avatar name="You" size={44} identity="slate" />
        <span class="caption">identity=slate ring + tint</span>
      </div>
      <div class="cell">
        <Avatar name="Priya" size={44} speaking />
        <span class="caption">speaking ring (quiet, neutral)</span>
      </div>
      <div class="cell">
        <Avatar name="Jordan Kim" size={64} src="https://i.pravatar.cc/128?img=12" />
        <span class="caption">image avatar, size 64</span>
      </div>
    </div>
  </section>

  <!-- ============================================================ -->
  <section>
    <h2>ParticipantTile — states</h2>
    <div class="row">
      <div class="cell">
        <div class="tile-frame"><ParticipantTile name="You" videoOn /></div>
        <span class="caption">video on</span>
      </div>
      <div class="cell">
        <div class="tile-frame"><ParticipantTile name="Sana" videoOn={false} /></div>
        <span class="caption">camera-off (plain minimal, no big initials)</span>
      </div>
      <div class="cell">
        <div class="tile-frame"><ParticipantTile name="Chantelle" videoOn speaking /></div>
        <span class="caption">speaking (quiet neutral ring, not identity-colored)</span>
      </div>
      <div class="cell">
        <div class="tile-frame"><ParticipantTile name="Marco" videoOn={false} muted /></div>
        <span class="caption">muted (neutral mic-off glyph chip)</span>
      </div>
      <div class="cell">
        <div class="tile-frame"><ParticipantTile name="Priya" videoOn weakConnection /></div>
        <span class="caption">weak connection dot</span>
      </div>
      <div class="cell">
        <div class="tile-frame">
          <ParticipantTile
            name="Devin"
            videoOn={false}
            ownerIdentity="devin"
            shareCount={2}
            sharingLiveBackground={identityColorCss('blue')}
            sharingLiveColor={identityInkCss('blue')}
          />
        </div>
        <span class="caption">#875 multi-share pill (count 2, interactive)</span>
      </div>
      <div class="cell">
        <div class="tile-frame">
          <ParticipantTile
            name="Marco"
            videoOn={false}
            ownerIdentity="marco"
            shareCount={12}
            sharingLiveBackground={identityColorCss('amber')}
            sharingLiveColor={identityInkCss('amber')}
          />
        </div>
        <span class="caption">#875 multi-share pill (count 12, capped "9+")</span>
      </div>
      <div class="cell">
        <div class="tile-frame">
          <ParticipantTile
            name="You"
            videoOn
            shareCount={3}
            sharingLiveBackground={identityColorCss('plum')}
            sharingLiveColor={identityInkCss('plum')}
            isLocal
          />
        </div>
        <span class="caption">#875 multi-share pill on local tile (non-interactive)</span>
      </div>
    </div>
  </section>

  <!-- ============================================================ -->
  <section>
    <h2>
      Gallery — full in-meeting layout
      <span class="section-count">
        <button class="count-btn" onclick={() => (galleryCount = Math.max(1, galleryCount - 1))} aria-label="fewer participants">−</button>
        <span class="count-val">{galleryCount}</span>
        <button class="count-btn" onclick={() => (galleryCount = Math.min(100, galleryCount + 1))} aria-label="more participants">+</button>
      </span>
    </h2>
    <div class="gallery-frame">
      <Gallery
        participants={galleryParticipants}
        micMuted={micMuted}
        cameraOn={camActive}
        sharingActive={shareActive}
        onOpenNetwork={() => {
          // #842: window.open() is a silent no-op inside the Tauri webview
          // (macOS wry has no new_window_req_handler registered) -- this dev
          // preview harness doesn't wire up the real open_network_cockpit_window
          // command, so stub the same way sibling controls below do.
          console.log('gallery: open network cockpit (stub)');
        }}
        onOpenDeviceMenu={(kind) => console.log('gallery: device menu', kind, '(stub)')}
        onControl={(icon) => {
          if (icon === 'mic') micMuted = !micMuted;
          else if (icon === 'camera') camActive = !camActive;
          else if (icon === 'screenshare') shareActive = !shareActive;
          else console.log('gallery control:', icon);
        }}
      />
    </div>
    <p class="note">Resize the browser window to confirm the tile grid reflows without breaking at small/large widths (SPEC.md §4.7).</p>
  </section>

  <!-- ============================================================ -->
  <section>
    <h2>
      Filmstrip — slim in-meeting companion (M1, SPEC.md §4.7)
      <span class="section-count">
        <button class="count-btn" onclick={() => (filmstripCount = Math.max(1, filmstripCount - 1))} aria-label="fewer participants">−</button>
        <span class="count-val">{filmstripCount}</span>
        <button class="count-btn" onclick={() => (filmstripCount = Math.min(100, filmstripCount + 1))} aria-label="more participants">+</button>
      </span>
    </h2>
    <div class="row">
      <div class="cell">
        <div class="filmstrip-frame filmstrip-frame-row">
          <Filmstrip participants={filmstripParticipants} orientation="row" />
        </div>
        <span class="caption">row (top/bottom strip) — default orientation</span>
      </div>
      <div class="cell">
        <div class="filmstrip-frame filmstrip-frame-column">
          <Filmstrip participants={filmstripParticipants} orientation="column" />
        </div>
        <span class="caption">column (side strip)</span>
      </div>
    </div>
    <p class="note">Reuses ParticipantTile at a small fixed size, not Gallery's reflowing grid — this is meant to float as thin chrome over the real shared-window content, per "faces are secondary furniture."</p>
  </section>

  <!-- ============================================================ -->
  <section>
    <h2>Pointer + NamePill — telepointer states</h2>
    <div class="row">
      <div class="cell">
        <div class="pointer-frame">
          <Pointer name="Priya" identity="amber" x={0.5} y={0.5} />
        </div>
        <span class="caption">moving (full opacity)</span>
      </div>
      <div class="cell">
        <div class="pointer-frame">
          <Pointer name="Chantelle" identity="plum" x={0.5} y={0.5} idle />
        </div>
        <span class="caption">idle (dimmed pointer + label)</span>
      </div>
      <div class="cell">
        <div class="pointer-frame">
          <Pointer name="Marco" identity="blue" x={0.5} y={0.5} pulseKey={pointerPulse} />
        </div>
        <button class="mini-btn" onclick={() => (pointerPulse += 1)}>trigger click ripple</button>
      </div>
      <div class="cell">
        <div class="pointer-frame">
          <Pointer name="Devin" identity="lilac" x={0.5} y={0.5} idle={pointerIdle} />
        </div>
        <button class="mini-btn" onclick={() => (pointerIdle = !pointerIdle)}>toggle idle: {pointerIdle}</button>
      </div>
    </div>
  </section>

  <!-- ============================================================ -->
  <section>
    <h2>RemoteWindowHeader — decoded shared-window design</h2>
    <div class="header-stack">
      <div class="header-frame">
        <RemoteWindowHeader
          ownerName="Chantelle Reyes"
          identity="plum"
          sourceTitle="main.rs — vscode"
          remoteControlActive={headerModes[0].remoteControl}
          drawActive={headerModes[0].draw}
          remoteControlAvailable
          onToggleRemoteControl={() => toggleHeaderRemoteControl(0)}
          onToggleDraw={() => toggleHeaderDraw(0)}
          onHideWindow={() => {}}
          onFitToSource={() => {}}
        />
      </div>
      <span class="note" style="margin-top:4px">traffic dots, generic app avatar, fixed View / Control / Draw segments — click to switch modes</span>

      <div class="header-frame">
        <RemoteWindowHeader
          ownerName="Marco"
          identity="blue"
          sourceTitle="dashboard.tsx — Chrome"
          remoteControlActive={headerModes[1].remoteControl}
          drawActive={headerModes[1].draw}
          remoteControlAvailable
          onToggleRemoteControl={() => toggleHeaderRemoteControl(1)}
          onToggleDraw={() => toggleHeaderDraw(1)}
          onHideWindow={() => {}}
          onFitToSource={() => {}}
        />
      </div>
      <span class="note" style="margin-top:4px">long title — drag the right edge to see how it collapses at narrow widths</span>

      <div class="header-frame">
        <RemoteWindowHeader
          ownerName="Sana"
          identity="green"
          sourceTitle="figma — spec review"
          focused
          remoteControlActive={headerModes[2].remoteControl}
          drawActive={headerModes[2].draw}
          remoteControlAvailable
          onToggleRemoteControl={() => toggleHeaderRemoteControl(2)}
          onToggleDraw={() => toggleHeaderDraw(2)}
          onHideWindow={() => {}}
          onFitToSource={() => {}}
        />
      </div>
      <span class="note" style="margin-top:4px">focused — stays revealed without a separate glow treatment</span>

      <div class="header-frame">
        <RemoteWindowHeader
          ownerName="Priya"
          identity="amber"
          sourceTitle="terminal — build output"
          remoteControlActive={headerModes[3].remoteControl}
          drawActive={headerModes[3].draw}
          remoteControlAvailable
          onToggleRemoteControl={() => toggleHeaderRemoteControl(3)}
          onToggleDraw={() => toggleHeaderDraw(3)}
          onHideWindow={() => {}}
          onFitToSource={() => {}}
        />
      </div>
      <span class="note" style="margin-top:4px">move the mouse away and wait ~1.8s: idle auto-hide to a sliver, reveals on hover</span>
    </div>
  </section>

  <!-- ============================================================ -->
  <section>
    <h2>MeetingChrome — large↔small transition (DESIGN.md §2)</h2>
    <div class="chrome-demo">
      <div class="chrome-frame">
        <MeetingChrome
          roomName="eng-sync"
          participants={galleryParticipants}
          activeIdentity="plum"
          micMuted={chromeMic}
          cameraOn={chromeCam}
          bind:expanded={chromeExpanded}
          pillHost={{ orientation: chromeOrientation }}
          localVideoStream={chromeFakeStream}
          onOpenNetwork={() => {
            // #842: window.open() is a silent no-op inside the Tauri webview
            // (macOS wry has no new_window_req_handler registered) -- this
            // dev preview harness doesn't wire up the real
            // open_network_cockpit_window command, so stub it like the
            // onControl handler below.
            console.log('chrome: open network cockpit (stub)');
          }}
          onControl={(icon) => {
            if (icon === 'mic') chromeMic = !chromeMic;
            else if (icon === 'camera') chromeCam = !chromeCam;
          }}
        />
      </div>
      <div class="chrome-controls">
        <button class="mini-btn" onclick={() => (chromeExpanded = !chromeExpanded)}>
          toggle expanded: {chromeExpanded}
        </button>
        <button
          class="mini-btn"
          onclick={() => (chromeOrientation = chromeOrientation === 'horizontal' ? 'vertical' : 'horizontal')}
        >
          pill orientation: {chromeOrientation}
        </button>
        <button class="mini-btn" onclick={toggleFakeStream}>
          fake camera stream: {chromeFakeStream ? 'on' : 'off'}
        </button>
      </div>
    </div>
    <p class="note">Click the layout toggle in the gallery topbar, or the extra circle in the pill, to spring between states. Drag the frame's resize handle narrower while collapsed to watch the pill's tail controls fold into a More menu (priority kept: Audio, Leave).</p>
  </section>

  <!-- ============================================================ -->
  <section>
    <h2>Button</h2>
    <div class="row">
      <div class="cell">
        <Button>Primary</Button>
        <span class="caption">primary (default)</span>
      </div>
      <div class="cell">
        <Button variant="ghost">Ghost</Button>
        <span class="caption">ghost</span>
      </div>
      <div class="cell">
        <Button disabled>Disabled</Button>
        <span class="caption">disabled</span>
      </div>
      <div class="cell">
        <div class="button-full-frame"><Button fullWidth>Full width</Button></div>
        <span class="caption">fullWidth</span>
      </div>
    </div>
  </section>

  <!-- ============================================================ -->
  <section>
    <h2>Checkbox</h2>
    <div class="row">
      <div class="cell">
        <Checkbox />
        <span class="caption">unchecked</span>
      </div>
      <div class="cell">
        <Checkbox bind:checked={checkboxChecked} />
        <button class="mini-btn" onclick={() => (checkboxChecked = !checkboxChecked)}>toggle: {checkboxChecked}</button>
        <span class="caption">interactive</span>
      </div>
      <div class="cell">
        <Checkbox checked={true} />
        <span class="caption">checked (static)</span>
      </div>
      <div class="cell">
        <Checkbox disabled />
        <span class="caption">disabled unchecked</span>
      </div>
      <div class="cell">
        <Checkbox checked={true} disabled />
        <span class="caption">disabled checked</span>
      </div>
    </div>
  </section>

  <!-- ============================================================ -->
  <section>
    <h2>Modal — scrim, Escape, focus trap (three widths)</h2>
    <div class="row">
      <div class="cell">
        <Button onclick={() => (openModal = 'compact')}>Open compact</Button>
        <span class="caption">width=compact (460px)</span>
      </div>
      <div class="cell">
        <Button onclick={() => (openModal = 'comfortable')}>Open comfortable</Button>
        <span class="caption">width=comfortable (640px, default)</span>
      </div>
      <div class="cell">
        <Button onclick={() => (openModal = 'wide')}>Open wide</Button>
        <span class="caption">width=wide (760px)</span>
      </div>
    </div>
    {#if openModal}
      <Modal title="Example modal" eyebrow="Dev harness" width={openModal} onClose={() => (openModal = null)}>
        <div class="modal-demo-body">
          <p class="modal-demo-p">Modal body content. Press Escape or click the backdrop to close.</p>
          <div class="modal-demo-actions">
            <Button variant="ghost" onclick={() => (openModal = null)}>Cancel</Button>
            <Button onclick={() => (openModal = null)}>Confirm</Button>
          </div>
        </div>
      </Modal>
    {/if}
  </section>

  <!-- ============================================================ -->
  <section>
    <h2>FeedbackModal — feedback form + log attachment + Modal shell</h2>
    <div class="row">
      <div class="cell">
        <Button onclick={() => (feedbackModalOpen = true)}>Open FeedbackModal</Button>
        <span class="caption">Escape / backdrop closes; log attachment opt-in; submit disabled while sharing</span>
      </div>
    </div>
    {#if feedbackModalOpen}
      <FeedbackModal onClose={() => (feedbackModalOpen = false)} />
    {/if}
  </section>

  <!-- ============================================================ -->
  <section>
    <h2>LiveHero — promoted live-room banner</h2>
    <div class="row">
      <div class="cell">
        <div class="hero-frame"><LiveHero roomName="eng-sync" participants={heroParticipants} onJoin={() => {}} /></div>
        <span class="caption">5 participants (4 faces + overflow)</span>
      </div>
      <div class="cell">
        <div class="hero-frame"><LiveHero roomName="design-review" participants={[]} onJoin={() => {}} /></div>
        <span class="caption">no participants</span>
      </div>
      <div class="cell">
        <div class="hero-frame"><LiveHero roomName="very-long-room-name-that-wraps-here" participants={heroParticipants.slice(0, 2)} onJoin={() => {}} /></div>
        <span class="caption">long name — full text, no truncation</span>
      </div>
    </div>
  </section>

  <!-- ============================================================ -->
  <section>
    <h2>RoomRow — main-menu list item states</h2>
    <p class="note" style="margin-bottom:16px">Hover each row to reveal Join, ★ favorite, and remove controls.</p>
    <div class="room-row-frame">
      <RoomRow
        name="eng-sync"
        accessCode="abc-123"
        participants={liveRoomParticipants}
        onJoin={() => {}}
        onToggleFavorite={() => {}}
        onCopyInvite={() => true}
        onRemove={() => {}}
      />
      <RoomRow
        name="design-review"
        accessCode="xyz-789"
        favorite
        onJoin={() => {}}
        onToggleFavorite={() => {}}
        onCopyInvite={() => true}
        onRemove={() => {}}
      />
      <RoomRow
        name="standup"
        current
        participants={currentRoomParticipants}
        onJoin={() => {}}
        onToggleFavorite={() => {}}
      />
      <RoomRow
        name="quiet-room"
        occupancy={7}
        onJoin={() => {}}
      />
    </div>
    <p class="note">↑ live (3 avatars) · empty + favorited · current/in-meeting · headcount-only (7 people)</p>
  </section>

  <!-- ============================================================ -->
  <section>
    <h2>MediaSplitControl — mic/camera toggle + options chevron (three sizes)</h2>
    <div class="row">
      <div class="cell">
        <MediaSplitControl
          icon="mic"
          active={mediaSplitMicActive}
          actionLabel="Toggle microphone"
          optionsLabel="Microphone options"
          size="gallery"
          visibleLabel="Mic"
          onToggle={() => (mediaSplitMicActive = !mediaSplitMicActive)}
          onOptions={() => {}}
        />
        <span class="caption">gallery (44px) — click to toggle</span>
      </div>
      <div class="cell">
        <MediaSplitControl
          icon="mic"
          active={mediaSplitMicActive}
          actionLabel="Toggle microphone"
          optionsLabel="Microphone options"
          size="pill"
          onToggle={() => (mediaSplitMicActive = !mediaSplitMicActive)}
          onOptions={() => {}}
        />
        <span class="caption">pill (40px)</span>
      </div>
      <div class="cell">
        <MediaSplitControl
          icon="camera"
          active={mediaSplitCamActive}
          actionLabel="Toggle camera"
          optionsLabel="Camera options"
          size="menubar"
          onToggle={() => (mediaSplitCamActive = !mediaSplitCamActive)}
          onOptions={() => {}}
        />
        <span class="caption">menubar / camera (32px)</span>
      </div>
      <div class="cell">
        <MediaSplitControl
          icon="mic"
          active={false}
          actionLabel="Toggle microphone"
          optionsLabel="Microphone options"
          size="gallery"
          optionsEnabled={false}
          visibleLabel="Mic"
          onToggle={() => {}}
        />
        <span class="caption">optionsEnabled=false (no chevron)</span>
      </div>
    </div>
  </section>

  <!-- ============================================================ -->
  <section>
    <h2>ContextMenu — right-click editing menu (globally mounted in root layout)</h2>
    <p class="note" style="margin-bottom:16px">Right-click the input to open the custom context menu (Cut / Copy / Paste / Select all). Select text first to enable Cut/Copy. Right-clicking non-editable selected text shows Copy only; empty chrome suppresses the menu.</p>
    <div class="row">
      <div class="cell">
        <input class="context-demo-input" type="text" value="Right-click me to edit" />
        <span class="caption">text input — select text, then right-click</span>
      </div>
    </div>
  </section>
</div>

<style>
  .harness {
    min-height: 100%;
    background: var(--bg-base);
    color: var(--text-primary);
    font-family: var(--font-ui);
    padding: 32px 40px 80px;
    overflow-y: auto;
  }

  h1 {
    font-family: var(--font-display);
    font-weight: 700;
    font-size: 28px;
    margin: 0 0 4px;
  }

  .intro {
    color: var(--text-muted);
    font-size: var(--text-body);
    margin: 0 0 32px;
  }

  section {
    margin-bottom: 40px;
  }

  h2 {
    font-size: 14px;
    font-weight: 700;
    color: var(--text-primary);
    border-bottom: 1px solid var(--hairline-strong);
    padding-bottom: 8px;
    margin: 0 0 20px;
  }

  .row {
    display: flex;
    flex-wrap: wrap;
    gap: 28px;
    align-items: flex-start;
  }

  .matrix {
    display: grid;
    grid-template-columns: 160px repeat(4, minmax(110px, auto));
    row-gap: 20px;
    column-gap: 16px;
    align-items: center;
  }

  .row-label {
    font-size: var(--text-caption);
    font-weight: 600;
    color: var(--text-faint);
  }

  .cell {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 8px;
  }

  .caption {
    font-size: 10.5px;
    font-family: var(--font-mono);
    color: var(--text-muted);
    text-align: center;
    max-width: 140px;
  }

  /* Section-level annotations: left-aligned, full width, same type treatment
     as .caption but not constrained to a component cell's narrow column. */
  .note {
    font-size: 10.5px;
    font-family: var(--font-mono);
    color: var(--text-muted);
    text-align: left;
    margin: 8px 0 0;
  }

  .dash {
    display: flex;
    align-items: center;
    justify-content: center;
    height: 44px;
    font-family: var(--font-mono);
    font-weight: 600;
    font-size: 15px;
    color: rgba(255, 255, 255, 0.25);
  }

  .hover-force :global(.control-button) {
    opacity: 0.88;
  }

  .pill-count {
    font-family: var(--font-mono);
    font-weight: 600;
    font-size: 10px;
    color: var(--text-muted);
  }

  .toast-text {
    font-size: var(--text-caption);
    font-weight: 500;
    color: rgba(255, 255, 255, 0.85);
  }

  .tile-frame {
    width: 200px;
    height: 130px;
  }

  .tile-frame :global(.tile) {
    width: 100%;
    height: 100%;
  }

  .gallery-frame {
    width: 100%;
    max-width: 960px;
    height: 560px;
    resize: both;
    overflow: auto;
    border: 1px dashed var(--hairline-strong);
  }

  .filmstrip-frame {
    background: var(--bg-base-2, var(--bg-base));
    border: 1px dashed var(--hairline-strong);
    padding: 20px;
    border-radius: var(--radius-tile);
  }

  .filmstrip-frame-row {
    width: 480px;
  }

  .filmstrip-frame-column {
    height: 260px;
  }

  .pointer-frame {
    position: relative;
    width: 140px;
    height: 90px;
    border-radius: var(--radius-tile);
    background: var(--surface);
    border: 1px solid var(--hairline-strong);
  }

  .header-stack {
    display: flex;
    flex-direction: column;
    gap: 16px;
    width: 480px;
    min-width: 200px;
    resize: horizontal;
    overflow: auto;
  }

  .header-frame {
    width: 100%;
    overflow: hidden;
    border-radius: var(--radius-tile);
    border: 1px solid var(--hairline-strong);
  }

  .chrome-demo {
    display: flex;
    align-items: flex-start;
    gap: 16px;
  }

  .chrome-controls {
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding-top: 4px;
  }

  .chrome-frame {
    position: relative;
    width: 480px;
    height: 360px;
    resize: both;
    min-width: 220px;
    min-height: 200px;
    border-radius: var(--radius-card);
    overflow: hidden;
    border: 1px solid var(--hairline-strong);
    background: var(--bg-base);
  }

  .mini-btn {
    font: 500 10.5px var(--font-mono);
    color: var(--text-muted);
    background: rgba(255, 255, 255, 0.06);
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-chip);
    padding: 4px 8px;
    cursor: pointer;
  }

  .mini-btn:hover {
    background: rgba(255, 255, 255, 0.1);
  }

  .button-full-frame {
    width: 200px;
  }

  .hero-frame {
    width: 360px;
    border-radius: var(--radius-card);
    overflow: hidden;
    border: 1px dashed var(--hairline-strong);
  }

  .room-row-frame {
    width: 360px;
    padding: 0 8px;
    display: flex;
    flex-direction: column;
    background: var(--menu-shell, var(--bg-base));
    border-radius: var(--radius-card);
    border: 1px dashed var(--hairline-strong);
  }

  .modal-demo-body {
    display: flex;
    flex-direction: column;
    gap: 20px;
    padding: 20px 18px 18px;
  }

  .modal-demo-p {
    margin: 0;
    font-size: 13px;
    color: var(--text-soft);
  }

  .modal-demo-actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
  }

  .context-demo-input {
    width: 240px;
    height: 38px;
    padding: 0 10px;
    border-radius: var(--radius-input);
    border: 1px solid var(--hairline-strong);
    background: var(--surface);
    color: var(--text-primary);
    font: 400 13px var(--font-ui);
  }

  .context-demo-input:focus-visible {
    outline: 2px solid var(--id-blue);
    outline-offset: 1px;
  }

  .section-count {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    margin-left: 10px;
    vertical-align: middle;
  }

  .count-btn {
    font: 600 12px var(--font-mono);
    color: var(--text-muted);
    background: rgba(255, 255, 255, 0.06);
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-chip);
    width: 20px;
    height: 20px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    cursor: pointer;
    line-height: 1;
  }

  .count-btn:hover {
    background: rgba(255, 255, 255, 0.1);
    color: var(--text-primary);
  }

  .count-val {
    font: 500 10.5px var(--font-mono);
    color: var(--text-muted);
    min-width: 14px;
    text-align: center;
  }
</style>
