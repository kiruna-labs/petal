// SINGLE SOURCE OF TRUTH for the token-server and invite-origin addresses
// a user can pick without rebuilding (#310). Desktop Settings, the browser
// client's home screen, and the native token client all share these rules
// so a URL that Settings accepts is the same origin Rust will call.
//
// An empty input means "no override": the launch-time env var, the URL
// baked into the build, or (web production only) the hosted default.
// Nothing here introduces a hosted default for a source build.

export const DEFAULT_INVITE_ORIGIN = 'https://meet.petal.live';
export const DEFAULT_WEB_BACKEND_ORIGIN = 'https://app.petal.live';

export const SERVER_BACKEND_TITLE = 'Token server';
export const SERVER_BACKEND_DESCRIPTION =
  'Address Petal uses to join meetings. Leave blank to use this build’s default. Applies the next time you join.';
export const SERVER_INVITE_TITLE = 'Invite links';
export const SERVER_INVITE_DESCRIPTION =
  'Origin copied invite links start with. Set this if you host your own meeting page.';
export const SERVER_SAVE_LABEL = 'Save';
export const SERVER_CLEAR_LABEL = 'Use default';
export const SERVER_INVALID_ORIGIN =
  'Enter an http:// or https:// address, origin only — no path.';
export const SERVER_ENV_OVERRIDE_NOTE =
  'This launch is using PETAL_BACKEND_URL from the environment, which overrides the saved server until Petal is opened without it.';

const MAX_ORIGIN_CHARS = 2048;
const STRIP_PATHS = new Set(['', '/api', '/api/token']);

export type BackendUrlSource = 'env' | 'settings' | 'build' | 'prod-default' | 'none';

export type ServerOriginParse =
  | { ok: true; origin: string | null }
  | { ok: false; error: string };

/**
 * Trim, require http(s), drop a trailing slash, and accept `/api` or
 * `/api/token` as a courtesy (the client appends those itself). Anything
 * else — other schemes, userinfo, or a real path — is rejected.
 */
export function normalizeServerOrigin(raw: string | null | undefined): string | null {
  if (raw == null) return null;
  const trimmed = raw.trim();
  if (!trimmed || trimmed.length > MAX_ORIGIN_CHARS) return null;
  let url: URL;
  try {
    url = new URL(trimmed);
  } catch {
    return null;
  }
  if (url.protocol !== 'http:' && url.protocol !== 'https:') return null;
  if (!url.hostname) return null;
  if (url.username || url.password) return null;
  const path = url.pathname.replace(/\/+$/, '');
  if (!STRIP_PATHS.has(path)) return null;
  return `${url.protocol}//${url.host}`;
}

/** Empty input clears the override. Non-empty input must normalize. */
export function parseServerOriginInput(raw: string): ServerOriginParse {
  if (raw.trim() === '') return { ok: true, origin: null };
  const origin = normalizeServerOrigin(raw);
  if (!origin) return { ok: false, error: SERVER_INVALID_ORIGIN };
  return { ok: true, origin };
}

export interface ResolveBackendBaseUrlInput {
  /** Process env (`PETAL_BACKEND_URL`). Empty string is a deliberate opt-out. */
  runtimeEnv?: string | null;
  /** Persisted user override, already normalized. */
  settingsUrl?: string | null;
  /** Compile-time bake (`option_env!` / `VITE_PETAL_BACKEND_URL`). */
  bakedUrl?: string | null;
  /**
   * Web production only: hosted fallback when nothing else is set.
   * Desktop must leave this unset — a source build has no hosted default.
   */
  productionDefault?: string | null;
}

export interface ResolvedBackendBaseUrl {
  url: string | null;
  source: BackendUrlSource;
}

function presentOrigin(raw: string | null | undefined): string | null {
  if (raw == null) return null;
  return normalizeServerOrigin(raw);
}

/**
 * Resolve the token-server origin. Runtime env wins so a launch helper
 * (`open --env PETAL_BACKEND_URL=…`) still works; an explicitly empty env
 * suppresses settings and the bake (same contract as `backend_base_url()`).
 */
export function resolveBackendBaseUrl(input: ResolveBackendBaseUrlInput): ResolvedBackendBaseUrl {
  if (input.runtimeEnv != null) {
    if (input.runtimeEnv.trim() === '') return { url: null, source: 'none' };
    const fromEnv = presentOrigin(input.runtimeEnv);
    if (fromEnv) return { url: fromEnv, source: 'env' };
  }
  const fromSettings = presentOrigin(input.settingsUrl);
  if (fromSettings) return { url: fromSettings, source: 'settings' };
  const fromBake = presentOrigin(input.bakedUrl);
  if (fromBake) return { url: fromBake, source: 'build' };
  const fromProd = presentOrigin(input.productionDefault);
  if (fromProd) return { url: fromProd, source: 'prod-default' };
  return { url: null, source: 'none' };
}

export interface ResolveInviteOriginInput {
  settingsOrigin?: string | null;
  bakedOrigin?: string | null;
  fallback: string;
}

export function resolveInviteOrigin(input: ResolveInviteOriginInput): string {
  return (
    presentOrigin(input.settingsOrigin) ??
    presentOrigin(input.bakedOrigin) ??
    presentOrigin(input.fallback) ??
    DEFAULT_INVITE_ORIGIN
  );
}

export function backendJoiningLabel(url: string | null, source: BackendUrlSource): string {
  if (!url) {
    return source === 'none'
      ? 'This build has no token server. Save one below, or set PETAL_BACKEND_URL when you launch.'
      : 'Not joining a token server this launch.';
  }
  if (source === 'env') return `Joining via ${url} (from PETAL_BACKEND_URL).`;
  if (source === 'settings') return `Joining via ${url}.`;
  return `Joining via ${url} (this build’s default).`;
}

/** Browser client: settings, then the Vite bake, then the hosted default in production. */
export function resolveWebTokenServerUrl(input: {
  settingsUrl?: string | null;
  bakedUrl?: string | null;
  isProduction?: boolean;
}): ResolvedBackendBaseUrl {
  return resolveBackendBaseUrl({
    settingsUrl: input.settingsUrl,
    bakedUrl: input.bakedUrl,
    productionDefault: input.isProduction ? DEFAULT_WEB_BACKEND_ORIGIN : null
  });
}

export function webBackendApiUrl(
  path: string,
  input: Parameters<typeof resolveWebTokenServerUrl>[0]
): string {
  return `${resolveWebTokenServerUrl(input).url ?? ''}${path}`;
}

/** Copied invite origin in the browser client: saved override, else this page. */
export function resolveWebInviteOrigin(
  settingsOrigin: string | null | undefined,
  pageOrigin: string
): string {
  return presentOrigin(settingsOrigin) ?? presentOrigin(pageOrigin) ?? pageOrigin.replace(/\/$/, '');
}
