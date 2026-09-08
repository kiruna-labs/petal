// The PLUGIN-BOOT Test Cockpit scenario (#37 / PR #82) decides its verdict by
// reading strings out of the host-side plugin journal. Those strings are
// written in TypeScript and parsed in Rust, so the two halves can drift
// silently: reword the diagnostic and the scenario stops finding its evidence,
// concludes "the frame never booted", and reports a product failure that is
// really a rename. Nothing else in the repo pins them together.
//
// This is a text lockstep on purpose. The Rust side is behind
// `--features cockpit-privileged`, which the frontend gate does not build, and
// the TS side needs a Tauri bridge to run at all -- so the one place both can
// be compared cheaply, on every PR, is their source.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const read = (relative: string) => readFileSync(new URL(relative, import.meta.url), 'utf8');

const adapter = read('../src/lib/plugins/tauriAdapter.ts');
const probePage = read('../src/routes/dev/plugin-boot/+page.svelte');
const rust = read('../src-tauri/src/test_cockpit/plugin_boot.rs');
const bus = read('../src-tauri/src/plugins/bus.rs');
const host = read('../../../shared/plugin-host/host.ts');

/** `const NAME: &str = "value";` -> value */
function rustConst(source: string, name: string): string {
  const match = source.match(new RegExp(`const ${name}: &str = "([^"]*)"`));
  assert.ok(match, `plugin_boot.rs no longer declares ${name}`);
  return match[1];
}

/** `const NAME = 'value';` -> value */
function tsConst(source: string, name: string): string {
  const match = source.match(new RegExp(`const ${name} = '([^']*)'`));
  assert.ok(match, `the probe page no longer declares ${name}`);
  return match[1];
}

test('the ready diagnostic the adapter writes is the one the Rust oracle parses', () => {
  // tauriAdapter.ts: hostLog('info', `plugin ${pluginId} frame ${event}`) for
  // 'ready' | 'activated'. That template is the whole evidence chain: only a
  // frame whose scripts ran and posted back to the broker produces it.
  assert.match(adapter, /event === 'ready' \|\| event === 'activated'/);
  assert.match(adapter, /hostLog\('info', `plugin \$\{pluginId\} frame \$\{event\}`\)/);

  // plugins/bus.rs slices exactly that shape back apart.
  assert.match(bus, /strip_prefix\("plugin "\)\?\.strip_suffix\(" frame ready"\)\?/);
});

test('the probe page and the PLUGIN-BOOT scenario agree on every journal marker', () => {
  assert.equal(tsConst(probePage, 'PROBE_MOUNTED_LINE'), rustConst(rust, 'PROBE_MOUNTED_LINE'));
  assert.equal(tsConst(probePage, 'SELF_NAV_LINE_PREFIX'), rustConst(rust, 'SELF_NAV_LINE_PREFIX'));

  // The page appends `; catalog=[…]` after the mounted marker, so the Rust
  // side has to match on a prefix rather than equality.
  assert.match(rust, /line\.starts_with\(PROBE_MOUNTED_LINE\)/);
  // …and `observed=true|false` is how the CSP observation is reported.
  assert.match(probePage, /frame-src violation observed=\$\{violated\}/);
  assert.match(rust, /line\.split\("observed="\)/);
});

test('the probe page mounts the real plugin host, not a lookalike', () => {
  // If this ever became a hand-rolled iframe the scenario would prove nothing
  // about the shipped plugin path.
  assert.match(probePage, /import PluginSurfaces from '\$lib\/plugins\/PluginSurfaces\.svelte'/);
  assert.match(probePage, /<PluginSurfaces/);
  assert.match(probePage, /import \{ installedPlugins \} from '\$lib\/plugins\/pluginCatalog'/);
});

test('the host boot line the scenario reads for "was it even loaded?" is unchanged', () => {
  // PluginSurfaces: hostLog('info', `host booted (Petal ${version}); loaded
  // plugins: …`). The scenario uses it to call an unloaded/disabled built-in
  // INFRA-FAIL instead of blaming WebKit for a frame nobody asked for.
  assert.match(
    read('../src/lib/plugins/PluginSurfaces.svelte'),
    /hostLog\('info', `host booted \(Petal \$\{version\}\); loaded plugins: /,
  );
  assert.match(rust, /const HOST_BOOTED_LINE_PREFIX: &str = "host booted"/);
});

test("the host's own never-ready canary still carries the phrase the scenario records", () => {
  // Corroborating detail in a PLUGIN-BOOT failure report, never the verdict.
  assert.match(host, /did not report ready/);
  assert.match(rust, /line\.contains\("did not report ready"\)/);
});
