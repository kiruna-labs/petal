<!--
  Dev-only visual QA harness for NetworkCockpit (issue #19).
  Sibling to /dev/secondary etc., same throwaway-scaffolding pattern.

  The seed snapshot/journal below are SYNTHETIC (clearly not live data —
  browser previews have no Tauri backend to answer get_network_snapshot).
  In the real app the component overwrites them with real stats on mount.
-->
<script lang="ts">
  import NetworkCockpit from '$lib/components/NetworkCockpit.svelte';
  import type { JournalEntry, NetworkSnapshot } from '$lib/ipc';

  // Synthetic 2-minute history: a plausible RTT/jitter/bandwidth session
  // with a mid-session wobble so the sparklines have visible shape.
  const t0 = Date.now() - 120_000;
  const history = Array.from({ length: 120 }, (_, i) => {
    const wobble = i > 60 && i < 80 ? 1 : 0;
    return {
      tMs: t0 + i * 1000,
      rttMs: 22 + Math.sin(i / 6) * 4 + wobble * 90,
      jitterMs: 2.5 + Math.abs(Math.sin(i / 9)) * 2 + wobble * 24,
      sendKbps: 3200 + Math.sin(i / 4) * 400 - wobble * 2200,
      recvKbps: 900 + Math.cos(i / 5) * 150,
      lossPct: wobble ? 3.4 : 0.0,
      glassToGlassEstimateMs: 34 + Math.abs(Math.sin(i / 8)) * 7 + wobble * 126,
      availableOutgoingKbps: 6200 - wobble * 2500,
      availableIncomingKbps: 9600 - wobble * 1600,
      cpuPct: 39 + Math.abs(Math.sin(i / 13)) * 8 + wobble * 18,
      memoryPct: 56 + Math.abs(Math.cos(i / 11)) * 5,
      thermalState: wobble ? 'fair' : 'nominal'
    };
  });
  const latestSample = history.at(-1);

  const snapshot: NetworkSnapshot = {
    connected: true,
    roomName: 'eng-sync',
    serverHost: 'localhost:7880',
    localIdentity: 'jordan-kim-1',
    reconnectCount: 1,
    quality: [
      { identity: 'jordan-kim-1', quality: 'excellent' },
      { identity: 'web-tester', quality: 'poor' }
    ],
    peerRttMs: 28.4,
    history,
    glassToGlassEstimateMs: latestSample?.glassToGlassEstimateMs ?? null,
    availableOutgoingKbps: latestSample?.availableOutgoingKbps ?? null,
    availableIncomingKbps: latestSample?.availableIncomingKbps ?? null,
    system: {
      cpuPct: latestSample?.cpuPct ?? null,
      memoryPct: latestSample?.memoryPct ?? null,
      thermalState: latestSample?.thermalState ?? null
    },
    nativeStartup: [],
    analysis: [
      {
        severity: 'warn',
        title: 'Packet loss is degrading media',
        evidence: 'Loss averaged 3.4% during the latest wobble.',
        recommendation: 'Check Wi-Fi interference, VPNs, and upstream congestion.'
      },
      {
        severity: 'warn',
        title: 'LiveKit reports poor participant quality',
        evidence: 'web-tester=poor',
        recommendation: 'Ask the affected participant to move closer to the router or switch networks.'
      }
    ],
    tracks: [
      {
        sid: 'TR_send01', name: 'petal-window-4242', rawTrackName: 'petal-window-4242',
        windowId: 4242, kind: 'video', direction: 'send',
        width: 1512, height: 844, fps: 30,
        codecImpl: 'SimulcastEncoderAdapter (VideoToolbox)', qualityLimitation: 'none',
        softwareEncoder: false, targetKbps: 4000, actualKbps: 3350, packetsLost: 2,
        framesEncoded: 1800, keyFramesEncoded: 12, framesDecoded: 0, keyFramesDecoded: 0,
        framesDropped: 0, nackCount: 8, firCount: 0, pliCount: 2, jitterBufferMs: null,
        glassToGlassMs: null, glassToGlassEstimateMs: null, streamState: 'active',
        grabbed: { width: 1512, height: 844, fps: 30, kbps: null },
        encodedSent: { width: 1512, height: 844, fps: 29.8, kbps: 3350 },
        received: null,
        decoded: null
      },
      {
        sid: 'TR_send02', name: 'microphone', kind: 'audio', direction: 'send',
        width: 0, height: 0, fps: 0, codecImpl: '', qualityLimitation: '',
        softwareEncoder: false, targetKbps: 0, actualKbps: 32, packetsLost: 0,
        framesEncoded: 0, keyFramesEncoded: 0, framesDecoded: 0, keyFramesDecoded: 0,
        framesDropped: 0, nackCount: 0, firCount: 0, pliCount: 0, jitterBufferMs: null,
        glassToGlassMs: null, glassToGlassEstimateMs: null, streamState: 'unknown'
      },
      {
        sid: 'TR_recv01', name: 'petal-window-981 (web-tester)', rawTrackName: 'petal-window-981',
        ownerIdentity: 'web-tester', windowId: 981, kind: 'video', direction: 'recv',
        width: 960, height: 600, fps: 24,
        codecImpl: 'VideoToolbox', qualityLimitation: '',
        softwareEncoder: false, targetKbps: 0, actualKbps: 870, packetsLost: 14,
        framesEncoded: 0, keyFramesEncoded: 0, framesDecoded: 1430, keyFramesDecoded: 9,
        framesDropped: 3, nackCount: 11, firCount: 0, pliCount: 3, jitterBufferMs: 41,
        jitterBufferTargetMs: 44, jitterBufferMinimumMs: 12,
        glassToGlassMs: null,
        glassToGlassEstimateMs: latestSample?.glassToGlassEstimateMs ?? 96,
        glassToGlassStatus: 'clock-sync-pending',
        streamState: 'active',
        grabbed: null,
        encodedSent: null,
        received: { width: 960, height: 600, fps: 24, kbps: 870 },
        decoded: { width: 960, height: 600, fps: 23.5, kbps: null },
        displayEnqueued: { width: 960, height: 600, fps: 21.9, kbps: null }
      }
    ]
  };

  const journal: JournalEntry[] = [
    { tMs: t0 - 4000, category: 'connection', message: "Joined room 'eng-sync' as 'jordan-kim-1'" },
    { tMs: t0 + 6000, category: 'presence', message: 'web-tester joined' },
    { tMs: t0 + 12_000, category: 'shares', message: 'Started publishing window 4242 share' },
    { tMs: t0 + 15_000, category: 'shares', message: 'Receiving window 981 share from web-tester' },
    { tMs: t0 + 40_000, category: 'media', message: 'jordan-kim-1 muted \'microphone\'' },
    { tMs: t0 + 62_000, category: 'connection', message: 'Reconnecting…' },
    { tMs: t0 + 63_400, category: 'connection', message: 'Reconnected' },
    { tMs: t0 + 66_000, category: 'connection', message: 'Connection quality for web-tester dropped to poor' }
  ];
</script>

<div class="harness">
  <h1>Petal — network cockpit dev harness</h1>
  <p class="intro">
    NetworkCockpit (issue #19) with SYNTHETIC seed data — a plain
    browser has no Tauri backend, so this previews layout only. In the real
    app the component replaces this with live stats on mount.
  </p>

  <section>
    <h2>Populated (synthetic)</h2>
    <div class="cell">
      <NetworkCockpit initialSnapshot={snapshot} initialJournal={journal} />
    </div>
  </section>

  <section>
    <h2>No data (browser preview / not in a meeting)</h2>
    <div class="cell">
      <NetworkCockpit />
    </div>
  </section>
</div>

<style>
  .harness {
    min-height: 100vh;
    padding: 32px 40px 80px;
    background: var(--bg-base);
    color: var(--text-primary);
    font-family: var(--font-ui);
  }
  h1 {
    font: 700 18px var(--font-ui);
    margin: 0 0 6px;
  }
  .intro {
    color: var(--text-muted);
    font-size: 12.5px;
    margin: 0 0 28px;
    max-width: 640px;
  }
  section {
    margin-bottom: 36px;
  }
  h2 {
    font: 600 13px var(--font-ui);
    color: var(--text-muted);
    margin: 0 0 14px;
  }
  .cell {
    display: flex;
  }
</style>
