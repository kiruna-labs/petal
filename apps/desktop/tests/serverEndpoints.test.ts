import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

import {
  DEFAULT_INVITE_ORIGIN,
  DEFAULT_WEB_BACKEND_ORIGIN,
  SERVER_HTTP_ORIGIN,
  SERVER_INVALID_ORIGIN,
  backendJoiningLabel,
  customServerIndicator,
  normalizeServerOrigin,
  parseServerOriginInput,
  resolveBackendBaseUrl,
  resolveInviteOrigin,
  resolveWebInviteOrigin,
  resolveWebTokenServerUrl,
  webBackendApiUrl
} from '@petal/shared/logic/serverEndpoints';
import { applyInviteOrigin, inviteLinkForAccessCode, inviteOrigin, INVITE_ORIGIN } from '../src/lib/data/inviteLinks.ts';
import { COMMANDS, EVENTS } from '../src/lib/ipc.ts';

test('#310: normalizeServerOrigin follows the shared serverOrigins fixture', () => {
  const contracts = JSON.parse(
    readFileSync(new URL('../../../contracts/petal-contracts.json', import.meta.url), 'utf8')
  ) as {
    serverOrigins: Array<{ input: string; origin: string | null; error?: string }>;
  };
  assert.ok(contracts.serverOrigins.length > 0);
  for (const row of contracts.serverOrigins) {
    assert.equal(normalizeServerOrigin(row.input), row.origin, row.input);
    if (row.error) {
      assert.deepEqual(parseServerOriginInput(row.input), { ok: false, error: row.error }, row.input);
    }
  }
  assert.equal(normalizeServerOrigin(''), null);
  assert.equal(normalizeServerOrigin(null), null);
  assert.equal(SERVER_HTTP_ORIGIN, 'http:// is only allowed for localhost, 127.0.0.1, and [::1].');
  assert.equal(
    SERVER_INVALID_ORIGIN,
    'Enter an https:// address, or http:// for localhost, 127.0.0.1, and [::1]. Origin only — no path.'
  );
});

test('#310: runtime PETAL_BACKEND_URL still accepts http for a non-loopback host', () => {
  assert.deepEqual(
    resolveBackendBaseUrl({ runtimeEnv: 'http://petal.example.test/api/token' }),
    { url: 'http://petal.example.test', source: 'env' }
  );
  assert.equal(normalizeServerOrigin('http://petal.example.test'), null);
});

test('#310: custom server indicator names the host only when Settings is the source', () => {
  assert.equal(
    customServerIndicator('https://petal.example.com', 'settings'),
    'Custom server: petal.example.com'
  );
  assert.equal(
    customServerIndicator('https://petal.example.com:8443', 'settings'),
    'Custom server: petal.example.com:8443'
  );
  assert.equal(customServerIndicator('https://petal.example.com', 'env'), null);
  assert.equal(customServerIndicator('https://petal.example.com', 'build'), null);
  assert.equal(customServerIndicator('https://petal.example.com', 'none'), null);
  assert.equal(customServerIndicator(null, 'settings'), null);
});

test('#310: parseServerOriginInput treats empty as clear and rejects junk', () => {
  assert.deepEqual(parseServerOriginInput('   '), { ok: true, origin: null });
  assert.deepEqual(parseServerOriginInput('https://meet.example.test/'), {
    ok: true,
    origin: 'https://meet.example.test'
  });
  assert.deepEqual(parseServerOriginInput('not-a-url'), {
    ok: false,
    error: SERVER_INVALID_ORIGIN
  });
});

test('#310: resolveBackendBaseUrl is env, then settings, then bake; empty env opts out', () => {
  assert.deepEqual(
    resolveBackendBaseUrl({
      runtimeEnv: '',
      settingsUrl: 'https://from-settings.example.test',
      bakedUrl: 'https://from-bake.example.test'
    }),
    { url: null, source: 'none' }
  );
  assert.deepEqual(
    resolveBackendBaseUrl({
      runtimeEnv: 'https://from-env.example.test',
      settingsUrl: 'https://from-settings.example.test',
      bakedUrl: 'https://from-bake.example.test'
    }),
    { url: 'https://from-env.example.test', source: 'env' }
  );
  assert.deepEqual(
    resolveBackendBaseUrl({
      settingsUrl: 'https://from-settings.example.test',
      bakedUrl: 'https://from-bake.example.test'
    }),
    { url: 'https://from-settings.example.test', source: 'settings' }
  );
  assert.deepEqual(
    resolveBackendBaseUrl({ bakedUrl: 'https://from-bake.example.test' }),
    { url: 'https://from-bake.example.test', source: 'build' }
  );
  assert.deepEqual(resolveBackendBaseUrl({}), { url: null, source: 'none' });
  assert.deepEqual(
    resolveBackendBaseUrl({ productionDefault: DEFAULT_WEB_BACKEND_ORIGIN }),
    { url: DEFAULT_WEB_BACKEND_ORIGIN, source: 'prod-default' }
  );
});

test('#310: a source desktop build still has no hosted token-server default', () => {
  assert.equal(resolveBackendBaseUrl({}).url, null);
  assert.notEqual(resolveBackendBaseUrl({ productionDefault: DEFAULT_WEB_BACKEND_ORIGIN }).source, 'none');
});

test('#310: invite origin falls through settings, bake, then the official host', () => {
  assert.equal(
    resolveInviteOrigin({
      settingsOrigin: 'https://meet.example.test',
      bakedOrigin: 'https://baked.example.test',
      fallback: DEFAULT_INVITE_ORIGIN
    }),
    'https://meet.example.test'
  );
  assert.equal(
    resolveInviteOrigin({
      bakedOrigin: 'https://baked.example.test',
      fallback: DEFAULT_INVITE_ORIGIN
    }),
    'https://baked.example.test'
  );
  assert.equal(resolveInviteOrigin({ fallback: DEFAULT_INVITE_ORIGIN }), DEFAULT_INVITE_ORIGIN);
});

test('#310: web token URL uses settings then bake then the production host', () => {
  assert.equal(
    webBackendApiUrl('/api/token', {
      settingsUrl: 'https://petal.example.test',
      bakedUrl: 'https://baked.example.test',
      isProduction: true
    }),
    'https://petal.example.test/api/token'
  );
  assert.equal(
    resolveWebTokenServerUrl({ bakedUrl: 'https://baked.example.test', isProduction: true }).source,
    'build'
  );
  assert.deepEqual(resolveWebTokenServerUrl({ isProduction: true }), {
    url: DEFAULT_WEB_BACKEND_ORIGIN,
    source: 'prod-default'
  });
  assert.deepEqual(resolveWebTokenServerUrl({ isProduction: false }), { url: null, source: 'none' });
});

test('#310: web invite origin uses the saved override, else this page', () => {
  assert.equal(
    resolveWebInviteOrigin('https://meet.example.test', 'https://localhost:5173'),
    'https://meet.example.test'
  );
  assert.equal(resolveWebInviteOrigin(null, 'https://localhost:5173'), 'https://localhost:5173');
});

test('#310: joining label names the source without truncating the URL', () => {
  assert.match(
    backendJoiningLabel('https://petal.example.test', 'settings'),
    /https:\/\/petal\.example\.test/
  );
  assert.match(backendJoiningLabel(null, 'none'), /no token server/);
});

test('#310: a Settings invite-origin override changes copied desktop links', () => {
  assert.equal(INVITE_ORIGIN, DEFAULT_INVITE_ORIGIN);
  try {
    applyInviteOrigin('https://meet.example.test');
    assert.equal(inviteOrigin(), 'https://meet.example.test');
    assert.equal(
      inviteLinkForAccessCode('Design Review!', 'abc-defg-hjk'),
      'https://meet.example.test/design-review/abc-defg-hjk'
    );
    applyInviteOrigin(null);
    assert.equal(inviteOrigin(), INVITE_ORIGIN);
    assert.equal(
      inviteLinkForAccessCode('Design Review!', 'abc-defg-hjk'),
      `${INVITE_ORIGIN}/design-review/abc-defg-hjk`
    );
  } finally {
    applyInviteOrigin(null);
  }
});

test('#310: server settings commands and event are registered in the IPC registry', () => {
  assert.equal(COMMANDS.serverSettings, 'server_settings');
  assert.equal(COMMANDS.setServerSettings, 'set_server_settings');
  assert.equal(EVENTS.serverSettingsChanged, 'server-settings-changed');
});

test('#310: server_settings module persists JSON and is registered on both platforms', () => {
  const source = readFileSync(
    new URL('../src-tauri/src/server_settings.rs', import.meta.url),
    'utf8'
  );
  assert.match(source, /pub struct ServerSettings/);
  assert.match(source, /pub backend_url: Option<String>/);
  assert.match(source, /pub invite_origin: Option<String>/);
  assert.match(source, /server-settings\.json/);
  assert.match(source, /pub fn server_settings\(\) -> ServerSettingsView/);
  assert.match(source, /pub fn set_server_settings\(/);
  assert.match(source, /SERVER_SETTINGS_CHANGED_EVENT: &str = "server-settings-changed"/);
  assert.match(source, /app\.emit\(SERVER_SETTINGS_CHANGED_EVENT/);

  const lib = readFileSync(new URL('../src-tauri/src/lib.rs', import.meta.url), 'utf8');
  assert.match(lib, /mod server_settings;/);
  const macRun = lib.split(
    '#[cfg(target_os = "macos")]\n#[cfg_attr(mobile, tauri::mobile_entry_point)]\npub fn run()'
  )[1];
  const otherRun = lib.split(
    '#[cfg(not(target_os = "macos"))]\n#[cfg_attr(mobile, tauri::mobile_entry_point)]\npub fn run()'
  )[1];
  assert.ok(macRun, 'macOS run() not found');
  assert.ok(otherRun, 'non-macOS run() not found');
  assert.match(macRun, /server_settings::initialize\(&app_data_dir\)/);
  assert.match(otherRun, /server_settings::initialize\(&app_data_dir\)/);

  const [macSection, otherSection] = lib.split('.invoke_handler(tauri::generate_handler![').slice(1);
  assert.ok(macSection, 'macOS invoke_handler list not found');
  assert.ok(otherSection, 'non-macOS invoke_handler list not found');
  assert.match(macSection, /server_settings::server_settings,/);
  assert.match(macSection, /server_settings::set_server_settings,/);
  assert.match(otherSection, /server_settings::server_settings,/);
  assert.match(otherSection, /server_settings::set_server_settings,/);
});

test('#310: token client consults the saved server after runtime env and before the bake', () => {
  const source = readFileSync(
    new URL('../src-tauri/src/transport/token.rs', import.meta.url),
    'utf8'
  );
  assert.match(source, /pub fn backend_base_url_source\(\)/);
  assert.match(source, /crate::server_settings::backend_url\(\)/);
  assert.match(
    source,
    /an explicitly empty runtime value is a deliberate local-LiveKit opt-out/i
  );
});

test('#310: Settings exposes Token server and Invite links fields that save through IPC', () => {
  const settings = readFileSync(
    new URL('../src/lib/components/Settings.svelte', import.meta.url),
    'utf8'
  );
  assert.match(settings, /SERVER_BACKEND_TITLE/);
  assert.match(settings, /SERVER_INVITE_TITLE/);
  assert.match(settings, /label: 'Server'/);
  assert.match(settings, /COMMANDS\.setServerSettings/);
  assert.match(settings, /COMMANDS\.serverSettings/);
  assert.match(settings, /handleSaveServerSettings/);
  assert.equal(
    settings.match(/void handleSaveServerSettings\(\)/g)?.length,
    1,
    'the Server section has one Save for both fields'
  );
  assert.match(settings, /overflow-wrap: anywhere/);
  assert.match(settings, /class="input server-url-input"/);
});

test('#310: every webview seeds invite origin from Rust and follows the change event', () => {
  const layout = readFileSync(new URL('../src/routes/+layout.svelte', import.meta.url), 'utf8');
  assert.match(layout, /applyInviteOrigin/);
  assert.match(layout, /COMMANDS\.serverSettings/);
  assert.match(layout, /EVENTS\.serverSettingsChanged/);
});

test('#310: the browser client has no Server panel and joins as on main', () => {
  const indexHtml = readFileSync(
    new URL('../../../web-harness/index.html', import.meta.url),
    'utf8'
  );
  const main = readFileSync(new URL('../../../web-harness/src/main.ts', import.meta.url), 'utf8');
  const connection = readFileSync(
    new URL('../../../web-harness/src/connection.ts', import.meta.url),
    'utf8'
  );
  const controls = readFileSync(
    new URL('../../../web-harness/src/controls.ts', import.meta.url),
    'utf8'
  );
  assert.doesNotMatch(indexHtml, /id="server-settings"/);
  assert.doesNotMatch(indexHtml, /id="backend-url-input"/);
  assert.doesNotMatch(main, /setupServerSettingsPanel/);
  assert.doesNotMatch(connection, /currentWebBackendApiUrl|serverSettings/);
  assert.match(connection, /https:\/\/app\.petal\.live/);
  assert.doesNotMatch(controls, /currentWebInviteOrigin/);

  const menu = readFileSync(
    new URL('../src/lib/components/MainMenu.svelte', import.meta.url),
    'utf8'
  );
  const gallery = readFileSync(
    new URL('../src/lib/components/Gallery.svelte', import.meta.url),
    'utf8'
  );
  const chrome = readFileSync(
    new URL('../src/lib/components/MeetingChrome.svelte', import.meta.url),
    'utf8'
  );
  const layout = readFileSync(new URL('../src/routes/+layout.svelte', import.meta.url), 'utf8');
  const mainPage = readFileSync(new URL('../src/routes/main/+page.svelte', import.meta.url), 'utf8');
  const meetingPage = readFileSync(
    new URL('../src/routes/meeting/[room]/+page.svelte', import.meta.url),
    'utf8'
  );
  const store = readFileSync(
    new URL('../src/lib/data/customServer.svelte.ts', import.meta.url),
    'utf8'
  );
  assert.match(menu, /data-testid="custom-server"/);
  assert.match(gallery, /data-testid="custom-server"/);
  assert.match(menu, /customServerLabel/);
  assert.match(gallery, /customServerLabel/);
  assert.match(chrome, /customServerLabel/);
  assert.match(layout, /noteCustomServer/);
  assert.match(mainPage, /customServerLabel=\{customServerState\.label\}/);
  assert.match(meetingPage, /customServerLabel=\{customServerState\.label\}/);
  assert.match(store, /customServerIndicator/);
});
