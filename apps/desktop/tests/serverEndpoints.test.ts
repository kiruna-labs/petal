import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

import {
  DEFAULT_INVITE_ORIGIN,
  DEFAULT_WEB_BACKEND_ORIGIN,
  SERVER_INVALID_ORIGIN,
  backendJoiningLabel,
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

test('#310: normalizeServerOrigin accepts http(s) origins and strips /api paths', () => {
  assert.equal(normalizeServerOrigin(' https://petal.example.test/// '), 'https://petal.example.test');
  assert.equal(
    normalizeServerOrigin('https://petal.example.test/api/token'),
    'https://petal.example.test'
  );
  assert.equal(normalizeServerOrigin('http://127.0.0.1:8787'), 'http://127.0.0.1:8787');
  assert.equal(normalizeServerOrigin('https://petal.example.test/extra'), null);
  assert.equal(normalizeServerOrigin('file:///tmp'), null);
  assert.equal(normalizeServerOrigin('https://user:pass@petal.example.test'), null);
  assert.equal(normalizeServerOrigin(''), null);
  assert.equal(normalizeServerOrigin(null), null);
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
  assert.match(settings, /overflow-wrap: anywhere/);
  assert.match(settings, /class="input server-url-input"/);
});

test('#310: every webview seeds invite origin from Rust and follows the change event', () => {
  const layout = readFileSync(new URL('../src/routes/+layout.svelte', import.meta.url), 'utf8');
  assert.match(layout, /applyInviteOrigin/);
  assert.match(layout, /COMMANDS\.serverSettings/);
  assert.match(layout, /EVENTS\.serverSettingsChanged/);
});

test('#310: browser client mirrors the same two fields on the home screen', () => {
  const indexHtml = readFileSync(
    new URL('../../../web-harness/index.html', import.meta.url),
    'utf8'
  );
  const main = readFileSync(new URL('../../../web-harness/src/main.ts', import.meta.url), 'utf8');
  const connection = readFileSync(
    new URL('../../../web-harness/src/connection.ts', import.meta.url),
    'utf8'
  );
  assert.match(indexHtml, /id="backend-url-input"/);
  assert.match(indexHtml, /id="invite-origin-input"/);
  assert.match(main, /setupServerSettingsPanel/);
  assert.match(connection, /currentWebBackendApiUrl/);
});
