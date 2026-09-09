// The plugin registry as the CLIENT sees it: a signed static `index.json`
// plus signed `bundle.json` files at stable versioned paths. Pinned by
// contracts/plugin-registry/ (a test keypair's public half, a signed sample
// index and bundle) which the marketplace publisher vendors byte-for-byte.
//
// This module is the index MODEL: shape validation and "what can this host
// install" -- consumed by the desktop's Settings browser on top of the index
// the Rust client (plugins/registry.rs) fetched and signature-verified. The
// two validators are pinned to each other by
// contracts/plugin-registry/invalid-index-cases.json, which both must reject
// case by case. Signature verification and download live only where the
// bytes are fetched (Rust today; the web client gets its own verifier with
// the install prompt in I-6, so no unused crypto ships before then).

import { HOST_API_VERSION, compareVersions, hostCompatibility, isPermission, isPluginId, isReleaseVersion, type Permission } from './manifest.ts';


export const REGISTRY_SCHEMA_VERSION = 1;
export const REGISTRY_INDEX_PATH = 'index.json';
export const REGISTRY_BUNDLE_MAX_BYTES = 2 * 1024 * 1024;
/** No registry existed before this; an older generatedAt is a replayed or forged index. */
export const REGISTRY_EPOCH_ISO = '2026-01-01T00:00:00.000Z';
const RFC3339_RE = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d{1,3})?Z$/;

export function isAcceptableGeneratedAt(value: string): boolean {
  if (!RFC3339_RE.test(value)) return false;
  const ms = Date.parse(value);
  return !Number.isNaN(ms) && ms >= Date.parse(REGISTRY_EPOCH_ISO);
}

/** Anti-rollback: an index is acceptable only if it is not older than the last one accepted. */
export function isNotRolledBack(previous: { generatedAtMs: number; signedAtS: number } | null, next: { generatedAtMs: number; signedAtS: number }): boolean {
  if (!previous) return true;
  return next.generatedAtMs >= previous.generatedAtMs && next.signedAtS >= previous.signedAtS;
}

export interface RegistryScan {
  tool: string;
  reportSha256: string;
  at: string;
}

export interface RegistryVersion {
  version: string;
  minHostVersion: string;
  apiVersion: number;
  permissions: Permission[];
  bundleUrl: string;
  sigUrl: string;
  sha256: string;
  size: number;
  verified: boolean;
  scan: RegistryScan | null;
}

export interface RegistryPlugin {
  id: string;
  name: string;
  description: string;
  publisher: string;
  latest: string;
  versions: RegistryVersion[];
}

export interface RegistryIndex {
  schemaVersion: typeof REGISTRY_SCHEMA_VERSION;
  generatedAt: string;
  plugins: RegistryPlugin[];
}

export type RegistryParse = { ok: true; index: RegistryIndex } | { ok: false; errors: string[] };

const SHA256_RE = /^[0-9a-f]{64}$/;

function isRecord(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}

export function isRegistryUrl(value: unknown): value is string {
  if (typeof value !== 'string') return false;
  try {
    const u = new URL(value);
    const local = u.hostname === 'localhost' || u.hostname === '127.0.0.1' || u.hostname === '[::1]';
    return u.protocol === 'https:' || (u.protocol === 'http:' && local);
  } catch {
    return false;
  }
}

export function bundlePath(id: string, version: string): string {
  if (!isPluginId(id)) throw new Error(`invalid plugin id: ${id}`);
  if (!isReleaseVersion(version)) throw new Error(`invalid version: ${version}`);
  return `plugins/${id}/${version}/bundle.json`;
}

/** Validate an untrusted index document. Whole-document failure on any bad entry: a registry is one signed artifact, not a stream. */
export function parseRegistryIndex(text: string): RegistryParse {
  const errors: string[] = [];
  let raw: unknown;
  try {
    raw = JSON.parse(text);
  } catch {
    return { ok: false, errors: ['index is not JSON'] };
  }
  if (!isRecord(raw)) return { ok: false, errors: ['index must be an object'] };
  if (raw.schemaVersion !== REGISTRY_SCHEMA_VERSION) errors.push(`schemaVersion must be ${REGISTRY_SCHEMA_VERSION}`);
  if (typeof raw.generatedAt !== 'string' || !isAcceptableGeneratedAt(raw.generatedAt)) {
    errors.push(`generatedAt must be an RFC 3339 date on or after ${REGISTRY_EPOCH_ISO}`);
  }
  if (!Array.isArray(raw.plugins)) return { ok: false, errors: [...errors, 'plugins must be an array'] };
  const plugins: RegistryPlugin[] = [];
  const seenIds = new Set<string>();
  raw.plugins.forEach((p, i) => {
    const where = `plugins[${i}]`;
    if (!isRecord(p)) return void errors.push(`${where}: must be an object`);
    if (!isPluginId(p.id)) return void errors.push(`${where}: bad id`);
    if (seenIds.has(p.id)) errors.push(`${where}: duplicate id ${p.id}`);
    seenIds.add(p.id);
    if (typeof p.name !== 'string' || p.name.length === 0 || p.name.length > 24) errors.push(`${where}: name must be 1..24 chars`);
    if (typeof p.description !== 'string' || p.description.length > 140) errors.push(`${where}: description must be ≤140 chars`);
    if (typeof p.publisher !== 'string' || p.publisher.length === 0 || p.publisher.length > 64) errors.push(`${where}: publisher required`);
    if (!isReleaseVersion(p.latest)) errors.push(`${where}: latest must be a release version`);
    if (!Array.isArray(p.versions) || p.versions.length === 0) return void errors.push(`${where}: versions must be a non-empty array`);
    const versions: RegistryVersion[] = [];
    const seenVersions = new Set<string>();
    p.versions.forEach((v, j) => {
      const vw = `${where}.versions[${j}]`;
      if (!isRecord(v)) return void errors.push(`${vw}: must be an object`);
      if (!isReleaseVersion(v.version)) errors.push(`${vw}: bad version`);
      else if (seenVersions.has(v.version)) errors.push(`${vw}: duplicate version`);
      if (typeof v.version === 'string') seenVersions.add(v.version);
      if (!isReleaseVersion(v.minHostVersion)) errors.push(`${vw}: bad minHostVersion`);
      if (typeof v.apiVersion !== 'number' || !Number.isInteger(v.apiVersion) || v.apiVersion < 1) errors.push(`${vw}: bad apiVersion`);
      if (!Array.isArray(v.permissions) || !v.permissions.every(isPermission) || new Set(v.permissions).size !== v.permissions.length) {
        errors.push(`${vw}: permissions must be known, unique permission strings`);
      }
      if (!isRegistryUrl(v.bundleUrl)) errors.push(`${vw}: bundleUrl must be https`);
      if (!isRegistryUrl(v.sigUrl)) errors.push(`${vw}: sigUrl must be https`);
      if (typeof v.sha256 !== 'string' || !SHA256_RE.test(v.sha256)) errors.push(`${vw}: sha256 must be 64 lowercase hex chars`);
      if (typeof v.size !== 'number' || !Number.isInteger(v.size) || v.size <= 0 || v.size > REGISTRY_BUNDLE_MAX_BYTES) errors.push(`${vw}: size must be 1..${REGISTRY_BUNDLE_MAX_BYTES}`);
      if (typeof v.verified !== 'boolean') errors.push(`${vw}: verified must be boolean`);
      if (v.scan !== null && v.scan !== undefined) {
        if (!isRecord(v.scan) || typeof v.scan.tool !== 'string' || typeof v.scan.reportSha256 !== 'string' || typeof v.scan.at !== 'string') {
          errors.push(`${vw}: scan must be null or {tool, reportSha256, at}`);
        }
      }
      versions.push({
        version: String(v.version),
        minHostVersion: String(v.minHostVersion),
        apiVersion: Number(v.apiVersion),
        permissions: Array.isArray(v.permissions) ? (v.permissions as Permission[]) : [],
        bundleUrl: String(v.bundleUrl),
        sigUrl: String(v.sigUrl),
        sha256: String(v.sha256),
        size: Number(v.size),
        verified: v.verified === true,
        scan: (v.scan as RegistryScan | null | undefined) ?? null,
      });
    });
    if (typeof p.latest === 'string' && !seenVersions.has(p.latest)) errors.push(`${where}: latest ${p.latest} is not among versions`);
    plugins.push({ id: String(p.id), name: String(p.name), description: String(p.description), publisher: String(p.publisher), latest: String(p.latest), versions });
  });
  if (errors.length) return { ok: false, errors };
  return { ok: true, index: { schemaVersion: REGISTRY_SCHEMA_VERSION, generatedAt: String(raw.generatedAt), plugins } };
}

/** The newest version this host can install from the UI: verified, compatible. */
export function installableVersion(plugin: RegistryPlugin, hostVersion: string, hostApiVersion = HOST_API_VERSION): RegistryVersion | null {
  const candidates = plugin.versions
    .filter((v) => v.verified && hostCompatibility({ apiVersion: v.apiVersion, minHostVersion: v.minHostVersion }, hostVersion, hostApiVersion).ok)
    .sort((a, b) => compareVersions(b.version, a.version));
  return candidates[0] ?? null;
}

export interface UpdateCandidate {
  id: string;
  from: string;
  to: RegistryVersion;
  /** Permissions the new version asks for that the installed one did not. */
  newPermissions: Permission[];
}

export function availableUpdates(
  installed: readonly { id: string; version: string; permissions: readonly Permission[] }[],
  index: RegistryIndex,
  hostVersion: string,
  hostApiVersion = HOST_API_VERSION,
): UpdateCandidate[] {
  const out: UpdateCandidate[] = [];
  for (const inst of installed) {
    const plugin = index.plugins.find((p) => p.id === inst.id);
    if (!plugin) continue;
    const best = installableVersion(plugin, hostVersion, hostApiVersion);
    if (!best || compareVersions(best.version, inst.version) <= 0) continue;
    out.push({ id: inst.id, from: inst.version, to: best, newPermissions: best.permissions.filter((p) => !inst.permissions.includes(p)) });
  }
  return out;
}
