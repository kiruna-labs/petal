// Token-server and invite-origin overrides for the browser client (#310).
// Desktop persists these in Rust (Settings is a separate webview); a tab is
// one JS realm, so localStorage is enough here. Resolution rules live in
// `shared/logic/serverEndpoints.ts` so a URL Settings accepts is the same
// origin this client will call.

import {
  SERVER_INVALID_ORIGIN,
  normalizeServerOrigin,
  parseServerOriginInput,
  resolveWebInviteOrigin,
  resolveWebTokenServerUrl,
  webBackendApiUrl,
  type ResolvedBackendBaseUrl,
} from '@petal/shared/logic/serverEndpoints';
import {
  HARNESS_BACKEND_URL_STORAGE_KEY,
  HARNESS_INVITE_ORIGIN_STORAGE_KEY,
} from './constants.ts';

export {
  HARNESS_BACKEND_URL_STORAGE_KEY,
  HARNESS_INVITE_ORIGIN_STORAGE_KEY,
} from './constants.ts';

export function readStoredOrigin(
  storage: Pick<Storage, 'getItem'> | null | undefined,
  key: string,
): string | null {
  if (!storage) return null;
  try {
    return normalizeServerOrigin(storage.getItem(key));
  } catch {
    return null;
  }
}

export function writeStoredOrigin(
  storage: Pick<Storage, 'setItem' | 'removeItem'>,
  key: string,
  origin: string | null,
): void {
  if (!origin) storage.removeItem(key);
  else storage.setItem(key, origin);
}

export function storedBackendUrl(
  storage: Pick<Storage, 'getItem'> | null | undefined = storageOrNull(),
): string | null {
  return readStoredOrigin(storage, HARNESS_BACKEND_URL_STORAGE_KEY);
}

export function storedInviteOrigin(
  storage: Pick<Storage, 'getItem'> | null | undefined = storageOrNull(),
): string | null {
  return readStoredOrigin(storage, HARNESS_INVITE_ORIGIN_STORAGE_KEY);
}

export function currentWebInviteOrigin(
  pageOrigin: string,
  storage: Pick<Storage, 'getItem'> | null | undefined = storageOrNull(),
): string {
  return resolveWebInviteOrigin(storedInviteOrigin(storage), pageOrigin);
}

export function currentWebTokenServer(
  input: { bakedUrl?: string | null; isProduction?: boolean },
  storage: Pick<Storage, 'getItem'> | null | undefined = storageOrNull(),
): ResolvedBackendBaseUrl {
  return resolveWebTokenServerUrl({
    settingsUrl: storedBackendUrl(storage),
    bakedUrl: input.bakedUrl,
    isProduction: input.isProduction,
  });
}

export function currentWebBackendApiUrl(
  path: string,
  input: { bakedUrl?: string | null; isProduction?: boolean },
  storage: Pick<Storage, 'getItem'> | null | undefined = storageOrNull(),
): string {
  return webBackendApiUrl(path, {
    settingsUrl: storedBackendUrl(storage),
    bakedUrl: input.bakedUrl,
    isProduction: input.isProduction,
  });
}

function storageOrNull(): Storage | null {
  try {
    return globalThis.localStorage ?? null;
  } catch {
    return null;
  }
}

export interface ServerSettingsPanelOptions {
  storage: Pick<Storage, 'getItem' | 'setItem' | 'removeItem'>;
  backendInput: HTMLInputElement;
  backendSave: HTMLButtonElement;
  backendClear: HTMLButtonElement;
  backendStatus: HTMLElement;
  inviteInput: HTMLInputElement;
  inviteSave: HTMLButtonElement;
  inviteClear: HTMLButtonElement;
  inviteStatus: HTMLElement;
  bakedBackendUrl?: string | null;
  isProduction?: boolean;
  pageOrigin: string;
}

function setStatus(el: HTMLElement, message: string, isError: boolean) {
  el.textContent = message;
  el.classList.toggle('error', isError);
}

export function setupServerSettingsPanel(options: ServerSettingsPanelOptions): void {
  const {
    storage,
    backendInput,
    backendSave,
    backendClear,
    backendStatus,
    inviteInput,
    inviteSave,
    inviteClear,
    inviteStatus,
    bakedBackendUrl,
    isProduction,
    pageOrigin,
  } = options;

  const savedBackend = storedBackendUrl(storage);
  const savedInvite = storedInviteOrigin(storage);
  backendInput.value = savedBackend ?? '';
  inviteInput.value = savedInvite ?? '';
  refreshBackendStatus();
  refreshInviteStatus();

  backendSave.addEventListener('click', () => {
    const parsed = parseServerOriginInput(backendInput.value);
    if (!parsed.ok) {
      setStatus(backendStatus, parsed.error, true);
      return;
    }
    writeStoredOrigin(storage, HARNESS_BACKEND_URL_STORAGE_KEY, parsed.origin);
    backendInput.value = parsed.origin ?? '';
    refreshBackendStatus();
  });
  backendClear.addEventListener('click', () => {
    writeStoredOrigin(storage, HARNESS_BACKEND_URL_STORAGE_KEY, null);
    backendInput.value = '';
    refreshBackendStatus();
  });
  inviteSave.addEventListener('click', () => {
    const parsed = parseServerOriginInput(inviteInput.value);
    if (!parsed.ok) {
      setStatus(inviteStatus, parsed.error, true);
      return;
    }
    writeStoredOrigin(storage, HARNESS_INVITE_ORIGIN_STORAGE_KEY, parsed.origin);
    inviteInput.value = parsed.origin ?? '';
    refreshInviteStatus();
  });
  inviteClear.addEventListener('click', () => {
    writeStoredOrigin(storage, HARNESS_INVITE_ORIGIN_STORAGE_KEY, null);
    inviteInput.value = '';
    refreshInviteStatus();
  });

  function refreshBackendStatus() {
    const resolved = currentWebTokenServer({ bakedUrl: bakedBackendUrl, isProduction }, storage);
    if (resolved.url) {
      setStatus(
        backendStatus,
        resolved.source === 'settings'
          ? `Joining via ${resolved.url}. Applies the next time you join.`
          : `Joining via ${resolved.url} (this build’s default).`,
        false,
      );
    } else {
      setStatus(
        backendStatus,
        'This build has no token server. Save one above, or this tab will use the local token path.',
        false,
      );
    }
  }

  function refreshInviteStatus() {
    const origin = currentWebInviteOrigin(pageOrigin, storage);
    setStatus(inviteStatus, `Copied links start with ${origin}.`, false);
  }
}

export { SERVER_INVALID_ORIGIN };
