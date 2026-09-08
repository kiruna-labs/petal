<!--
  Plugin-boot probe (#37 / PR #82). The ONE thing this page exists to prove:
  that a plugin's sandboxed `srcdoc` iframe actually boots inside WKWebView
  under the embedder CSP the desktop app ships (`frame-src 'none'` in
  `src-tauri/tauri.conf.json`). Per CSP3 `about:srcdoc` is exempt from
  `frame-src` matching — confirmed in Chromium by
  `web-harness/tests/pluginSelfNavigation.test.ts`, but Chromium is not what
  the shipped macOS app renders with, and nothing else covers WebKit.

  It mounts the REAL `PluginSurfaces` with the REAL built-in catalog, so the
  frame, its srcdoc, its sandbox attributes and the broker handshake are the
  shipped code paths, not a lookalike. The Test Cockpit's PLUGIN-BOOT scenario
  (src-tauri/src/test_cockpit/plugin_boot.rs) opens this route and asserts on
  the host-side journal — `plugins(host): plugin petal.reactions frame ready`,
  which can only be written after the frame's own scripts ran and posted back.

  Every line this page emits through `hostLog` is host-side evidence in
  `~/Library/Logs/Petal/petal.log`; keep the wording in step with the Rust
  markers in `plugin_boot.rs` (`PROBE_MOUNTED_LINE`, `SELF_NAV_LINE_PREFIX`).
-->
<script lang="ts">
  import { onMount } from 'svelte';
  import { getVersion } from '@tauri-apps/api/app';
  import type { Participant } from '@petal/shared/plugin-host/api';
  import type { ToolbarButtonModel } from '@petal/shared/plugin-host/surfaces';
  import PluginSurfaces from '$lib/plugins/PluginSurfaces.svelte';
  import { hostLog } from '$lib/plugins/tauriAdapter';
  import { installedPlugins, type CatalogEntry } from '$lib/plugins/pluginCatalog';

  /** Kept byte-identical with `plugin_boot::PROBE_MOUNTED_LINE`. */
  const PROBE_MOUNTED_LINE = 'plugin-boot probe: page mounted';
  /** Kept byte-identical with `plugin_boot::SELF_NAV_LINE_PREFIX`. */
  const SELF_NAV_LINE_PREFIX = 'plugin-boot probe: srcdoc self-navigation';
  /** Unresolvable on purpose: this must never reach a network, only the CSP check. */
  const SELF_NAV_TARGET = 'https://plugin-boot-probe.invalid/';
  const SELF_NAV_SETTLE_MS = 2500;

  let hostVersion = $state<string | null>(null);
  let buttons = $state<ToolbarButtonModel[]>([]);
  let selfNav = $state('running…');

  // A single local participant so the host has a plausible meeting snapshot.
  // The room is real during a cockpit run (the engine joins before scenarios),
  // so advert publishing takes its normal path rather than a stubbed one.
  const participants: Participant[] = [
    { identity: 'plugin-boot-probe', name: 'Plugin boot probe', isLocal: true, speaking: false, micMuted: true }
  ];

  // Async since I-5a: registry installs come from the Rust store. The
  // mounted line (which the cockpit waits for) is logged once it is known.
  let catalog = $state<CatalogEntry[]>([]);

  /**
   * The NEGATIVE direction, and the only half a page can observe about its
   * own policy: park a sandboxed srcdoc frame on a cross-origin URL and see
   * whether the embedder's `frame-src 'none'` refuses the navigation. A
   * refusal fires `securitypolicyviolation` on THIS document (the policy
   * that was violated is the embedder's), so a violation is positive
   * evidence of enforcement — its absence is not evidence of anything, and
   * the scenario deliberately does not gate its verdict on this line.
   */
  function probeSelfNavigation(): void {
    let violated = false;
    let blockedUri = '';
    const onViolation = (event: SecurityPolicyViolationEvent) => {
      const directive = event.effectiveDirective || event.violatedDirective || '';
      if (directive.startsWith('frame-src')) {
        violated = true;
        blockedUri = event.blockedURI || '';
      }
    };
    document.addEventListener('securitypolicyviolation', onViolation);

    const frame = document.createElement('iframe');
    frame.setAttribute('sandbox', 'allow-scripts');
    frame.setAttribute('title', 'plugin-boot self-navigation probe');
    frame.hidden = true;
    frame.srcdoc =
      '<!doctype html><meta charset="utf-8"><script>location.href=' +
      JSON.stringify(SELF_NAV_TARGET) +
      '<\/script>';
    document.body.appendChild(frame);

    setTimeout(() => {
      document.removeEventListener('securitypolicyviolation', onViolation);
      frame.remove();
      selfNav = violated ? `refused (blockedURI=${blockedUri || 'unreported'})` : 'no frame-src violation reported';
      hostLog(
        'info',
        `${SELF_NAV_LINE_PREFIX} frame-src violation observed=${violated} blockedUri=${blockedUri || 'none'}`
      );
    }, SELF_NAV_SETTLE_MS);
  }

  onMount(() => {
    void installedPlugins((message) => hostLog('warn', `plugin-boot probe: ${message}`)).then((entries) => {
      catalog = entries;
      hostLog('info', `${PROBE_MOUNTED_LINE}; catalog=[${catalog.map((p) => p.manifest.id).join(', ') || 'empty'}]`);
    });
    // A non-numeric version still boots built-ins (PluginSurfaces skips the
    // compatibility gate for it), so a denied `getVersion` degrades the
    // evidence rather than voiding the probe.
    void getVersion()
      .then((version) => (hostVersion = version))
      .catch((error) => {
        hostLog('warn', `plugin-boot probe: getVersion failed (${String(error)}); booting as "dev"`);
        hostVersion = 'dev';
      });
    probeSelfNavigation();
  });
</script>

<div class="probe">
  <h1>Plugin boot probe</h1>
  <dl>
    <dt>Host version</dt>
    <dd>{hostVersion ?? 'resolving…'}</dd>
    <dt>Catalog</dt>
    <dd>{catalog.map((p) => p.manifest.id).join(', ') || 'empty'}</dd>
    <dt>Host-drawn toolbar buttons</dt>
    <dd>{buttons.map((b) => `${b.pluginId}/${b.buttonId}`).join(', ') || 'none'}</dd>
    <dt>srcdoc self-navigation</dt>
    <dd>{selfNav}</dd>
  </dl>
  <p>
    The verdict is not read from this page. It is read from the host-side plugin journal
    (<code>plugins(host): plugin &lt;id&gt; frame ready</code>) by the Test Cockpit's PLUGIN-BOOT scenario.
  </p>

  <PluginSurfaces
    {participants}
    roomLabel="plugin-boot probe"
    phase="connected"
    {hostVersion}
    bind:buttons
    onToast={(text) => hostLog('info', `plugin-boot probe: toast ${text}`)}
  />
</div>

<style>
  .probe {
    position: relative;
    display: flex;
    flex-direction: column;
    gap: 10px;
    padding: 14px;
    color: var(--text-primary);
    background: var(--bg-base);
    font-family: var(--font-ui);
    font-size: var(--text-body);
  }

  h1 {
    margin: 0;
    font-size: var(--text-hero);
  }

  dl {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: 4px 12px;
    margin: 0;
  }

  dt {
    color: var(--text-muted);
    font-family: var(--font-mono);
    font-size: var(--text-micro);
  }

  dd {
    margin: 0;
    overflow-wrap: anywhere;
  }

  p {
    margin: 0;
    color: var(--text-muted);
    font-size: var(--text-micro);
  }

  code {
    font-family: var(--font-mono);
  }
</style>
