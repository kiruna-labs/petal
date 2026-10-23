import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

import {
  DEFAULT_WEB_BACKEND_ORIGIN,
  SERVER_INVALID_ORIGIN,
  normalizeServerOrigin,
  parseServerOriginInput,
  resolveWebInviteOrigin,
  webBackendApiUrl
} from '@petal/shared/logic/serverEndpoints';
import {
  HARNESS_BACKEND_URL_STORAGE_KEY,
  HARNESS_INVITE_ORIGIN_STORAGE_KEY,
  currentWebInviteOrigin,
  currentWebBackendApiUrl,
  readStoredOrigin,
  setupServerSettingsPanel,
  storedBackendUrl,
  writeStoredOrigin
} from '../src/serverSettings.ts';

class MemoryStorage {
  values = new Map<string, string>();

  getItem(key: string): string | null {
    return this.values.get(key) ?? null;
  }

  setItem(key: string, value: string): void {
    this.values.set(key, value);
  }

  removeItem(key: string): void {
    this.values.delete(key);
  }
}

class FakeClassList {
  private readonly values = new Set<string>();
  add(name: string) {
    this.values.add(name);
  }
  remove(name: string) {
    this.values.delete(name);
  }
  toggle(name: string, force?: boolean): boolean {
    const next = force ?? !this.values.has(name);
    if (next) this.values.add(name);
    else this.values.delete(name);
    return next;
  }
  contains(name: string): boolean {
    return this.values.has(name);
  }
}

class FakeEl {
  value = '';
  textContent = '';
  classList = new FakeClassList();
  private readonly listeners = new Map<string, () => void>();
  addEventListener(type: string, listener: () => void) {
    this.listeners.set(type, listener);
  }
  click() {
    this.listeners.get('click')?.();
  }
}

test('#310: shared origin parser is the same module the desktop Settings uses', () => {
  assert.equal(normalizeServerOrigin('https://petal.example.test/api'), 'https://petal.example.test');
  assert.deepEqual(parseServerOriginInput('ftp://nope'), { ok: false, error: SERVER_INVALID_ORIGIN });
});

test('#310: stored origins round-trip through localStorage and reject junk', () => {
  const storage = new MemoryStorage();
  writeStoredOrigin(storage, HARNESS_BACKEND_URL_STORAGE_KEY, 'https://petal.example.test/api/token');
  assert.equal(storage.getItem(HARNESS_BACKEND_URL_STORAGE_KEY), 'https://petal.example.test/api/token');
  assert.equal(readStoredOrigin(storage, HARNESS_BACKEND_URL_STORAGE_KEY), 'https://petal.example.test');
  writeStoredOrigin(storage, HARNESS_BACKEND_URL_STORAGE_KEY, null);
  assert.equal(storedBackendUrl(storage), null);
});

test('#310: currentWebBackendApiUrl prefers the saved server over the bake', () => {
  const storage = new MemoryStorage();
  writeStoredOrigin(storage, HARNESS_BACKEND_URL_STORAGE_KEY, 'https://petal.example.test');
  assert.equal(
    currentWebBackendApiUrl('/api/token', { bakedUrl: 'https://baked.example.test', isProduction: true }, storage),
    'https://petal.example.test/api/token'
  );
  writeStoredOrigin(storage, HARNESS_BACKEND_URL_STORAGE_KEY, null);
  assert.equal(
    currentWebBackendApiUrl('/api/token', { bakedUrl: 'https://baked.example.test', isProduction: true }, storage),
    'https://baked.example.test/api/token'
  );
  assert.equal(
    webBackendApiUrl('/api/token', { isProduction: true }),
    `${DEFAULT_WEB_BACKEND_ORIGIN}/api/token`
  );
});

test('#310: copied invite links follow the saved origin, else this page', () => {
  const storage = new MemoryStorage();
  assert.equal(currentWebInviteOrigin('https://localhost:5173', storage), 'https://localhost:5173');
  writeStoredOrigin(storage, HARNESS_INVITE_ORIGIN_STORAGE_KEY, 'https://meet.example.test');
  assert.equal(currentWebInviteOrigin('https://localhost:5173', storage), 'https://meet.example.test');
  assert.equal(resolveWebInviteOrigin(null, 'https://meet.petal.live'), 'https://meet.petal.live');
});

test('#310: home-screen Server panel saves and clears through the shared parser', () => {
  const storage = new MemoryStorage();
  const backendInput = new FakeEl();
  const inviteInput = new FakeEl();
  const backendStatus = new FakeEl();
  const inviteStatus = new FakeEl();
  const backendSave = new FakeEl();
  const backendClear = new FakeEl();
  const inviteSave = new FakeEl();
  const inviteClear = new FakeEl();

  setupServerSettingsPanel({
    storage,
    backendInput: backendInput as unknown as HTMLInputElement,
    backendSave: backendSave as unknown as HTMLButtonElement,
    backendClear: backendClear as unknown as HTMLButtonElement,
    backendStatus: backendStatus as unknown as HTMLElement,
    inviteInput: inviteInput as unknown as HTMLInputElement,
    inviteSave: inviteSave as unknown as HTMLButtonElement,
    inviteClear: inviteClear as unknown as HTMLButtonElement,
    inviteStatus: inviteStatus as unknown as HTMLElement,
    bakedBackendUrl: null,
    isProduction: false,
    pageOrigin: 'https://localhost:5173'
  });

  backendInput.value = 'not a url';
  backendSave.click();
  assert.equal(storedBackendUrl(storage), null);
  assert.equal(backendStatus.textContent, SERVER_INVALID_ORIGIN);

  backendInput.value = 'https://petal.example.test/api';
  backendSave.click();
  assert.equal(storedBackendUrl(storage), 'https://petal.example.test');
  assert.match(backendStatus.textContent, /petal\.example\.test/);

  backendClear.click();
  assert.equal(storedBackendUrl(storage), null);
  assert.equal(backendInput.value, '');

  inviteInput.value = 'https://meet.example.test';
  inviteSave.click();
  assert.equal(currentWebInviteOrigin('https://localhost:5173', storage), 'https://meet.example.test');
  inviteClear.click();
  assert.equal(currentWebInviteOrigin('https://localhost:5173', storage), 'https://localhost:5173');
});

test('#310: connection.ts and invite copy use the shared server helpers', () => {
  const connection = readFileSync(new URL('../src/connection.ts', import.meta.url), 'utf8');
  const controls = readFileSync(new URL('../src/controls.ts', import.meta.url), 'utf8');
  const homeScreen = readFileSync(new URL('../src/homeScreen.ts', import.meta.url), 'utf8');
  const indexHtml = readFileSync(new URL('../index.html', import.meta.url), 'utf8');
  const main = readFileSync(new URL('../src/main.ts', import.meta.url), 'utf8');

  assert.match(connection, /currentWebBackendApiUrl/);
  assert.match(controls, /currentWebInviteOrigin\(location\.origin\)/);
  assert.match(homeScreen, /currentWebInviteOrigin\(location\.origin\)/);
  assert.match(indexHtml, /id="server-settings"/);
  assert.match(indexHtml, /id="backend-url-input"/);
  assert.match(indexHtml, /id="invite-origin-input"/);
  assert.match(main, /setupServerSettingsPanel/);
});
