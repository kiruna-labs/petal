// SINGLE SOURCE OF TRUTH for the plugin manifest: its TypeScript shape, the
// permission vocabulary, size limits, and the validator both clients run
// before a plugin bundle is allowed anywhere near a frame. Consumed by the
// desktop app, the web client, and (as a type re-export) `@petal/plugin-sdk`.
// Design: plugins/README.md §2.2 and §2.8.

export const MANIFEST_VERSION = 1 as const;

/** The plugin API major this host implements. Bumped only on breaking changes. */
export const HOST_API_VERSION = 1;

export type PluginScope = 'meeting' | 'local';

export const STATIC_PERMISSIONS = [
  'meeting:read',
  'data:publish',
  'state:write',
  'storage',
  'ui:toolbar-button',
  'ui:header-button',
  'ui:overlay',
  'ui:popover',
  'ui:panel',
  'ui:settings',
  'ui:toast',
  'shares:read',
  'clipboard:write',
  'net:fetch:user-urls',
] as const;
export type StaticPermission = (typeof STATIC_PERMISSIONS)[number];
/** `net:fetch:<host>` — exact host or `*.example.com`. Never `net:fetch:*`. */
export type NetHostPermission = `net:fetch:${string}`;
export type Permission = StaticPermission | NetHostPermission;

/** Known to the vocabulary, refused by this host until the feature ships. */
export const RESERVED_PERMISSIONS = ['frames:read'] as const;

/** Only a meeting-scoped plugin may talk to other participants. */
export const MEETING_ONLY_PERMISSIONS: readonly Permission[] = ['data:publish', 'state:write'];

export const MANIFEST_LIMITS = {
  idMaxLength: 64,
  /** Must fit a 400 px Settings row beside a version chip and a toggle. */
  nameMaxLength: 24,
  descriptionMaxLength: 140,
  /** Header/toolbar labels; longer labels collapse to icon-only anyway. */
  buttonLabelMaxLength: 14,
  contributionIdMaxLength: 32,
  bundleMaxBytes: 2 * 1024 * 1024,
} as const;

export const PLUGIN_ID_RE = /^[a-z0-9]+(\.[a-z0-9-]+)+$/;
const CONTRIBUTION_ID_RE = /^[a-z0-9][a-z0-9-]*$/;
const RELEASE_VERSION_RE = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/;
const ENTRY_RE = /^[A-Za-z0-9_.-]+\.js$/;
const NET_HOST_RE = /^(\*\.)?([a-z0-9-]+\.)*[a-z0-9-]+(:\d{1,5})?$/;
const ICON_RE = /^[a-z][a-z0-9-]{0,31}$/;

export type SurfaceKind = 'overlay' | 'popover' | 'panel' | 'settings';
export const SURFACE_KINDS: readonly SurfaceKind[] = ['overlay', 'popover', 'panel', 'settings'];
const SURFACE_PERMISSION: Record<SurfaceKind, StaticPermission> = {
  overlay: 'ui:overlay',
  popover: 'ui:popover',
  panel: 'ui:panel',
  settings: 'ui:settings',
};

export interface ToolbarButtonContribution {
  id: string;
  label: string;
  icon: string;
  /** `"<surfaceKind>:<surfaceId>"` the host opens on click; otherwise the click is an action. */
  opens?: string;
}
export interface HeaderButtonContribution {
  id: string;
  label: string;
  icon: string;
}
export interface SurfaceContribution {
  id: string;
  width?: number;
  height?: number;
  title?: string;
}
export type SettingsFieldType = 'text' | 'url' | 'boolean';
export interface SettingsFieldContribution {
  key: string;
  type: SettingsFieldType;
  label: string;
  /** `url` fields only: the value's origin joins the plugin's fetch allowlist. */
  netAllow?: boolean;
}
export interface PluginContributions {
  toolbarButtons?: ToolbarButtonContribution[];
  headerButtons?: HeaderButtonContribution[];
  surfaces?: Partial<Record<SurfaceKind, SurfaceContribution | null>>;
  settings?: SettingsFieldContribution[];
}
/** Reserved for the future Rust-hosted WASM tier. Accepted, never executed, by this host. */
export interface NativeSlot {
  wasm: string;
  abi: string;
  capabilities?: string[];
}

export interface PluginManifest {
  manifestVersion: typeof MANIFEST_VERSION;
  id: string;
  version: string;
  name: string;
  description: string;
  apiVersion: number;
  minHostVersion: string;
  scope: PluginScope;
  entry: string;
  permissions: Permission[];
  contributes?: PluginContributions;
  native?: NativeSlot | null;
}

export type ManifestValidation =
  | { ok: true; manifest: PluginManifest; warnings: string[] }
  | { ok: false; errors: string[] };

export function isPluginId(value: unknown): value is string {
  return typeof value === 'string' && value.length <= MANIFEST_LIMITS.idMaxLength && PLUGIN_ID_RE.test(value);
}

/** Strict `major.minor.patch`; prerelease/build suffixes are refused for plugins. */
export function isReleaseVersion(value: unknown): value is string {
  return typeof value === 'string' && RELEASE_VERSION_RE.test(value);
}

/** Returns <0, 0, >0. Inputs must satisfy `isReleaseVersion`. */
export function compareVersions(a: string, b: string): number {
  const pa = a.split('.').map(Number);
  const pb = b.split('.').map(Number);
  for (let i = 0; i < 3; i++) {
    if (pa[i] !== pb[i]) return pa[i]! < pb[i]! ? -1 : 1;
  }
  return 0;
}

export function isStaticPermission(value: string): value is StaticPermission {
  return (STATIC_PERMISSIONS as readonly string[]).includes(value);
}

export function isNetHostPermission(value: string): value is NetHostPermission {
  if (!value.startsWith('net:fetch:')) return false;
  const host = value.slice('net:fetch:'.length);
  return host !== 'user-urls' && host !== '*' && NET_HOST_RE.test(host);
}

export function isPermission(value: unknown): value is Permission {
  return typeof value === 'string' && (isStaticPermission(value) || isNetHostPermission(value));
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function checkContributionId(errors: string[], where: string, id: unknown, seen: Set<string>): void {
  if (typeof id !== 'string' || id.length > MANIFEST_LIMITS.contributionIdMaxLength || !CONTRIBUTION_ID_RE.test(id)) {
    errors.push(`${where}: id must match ${CONTRIBUTION_ID_RE} and be at most ${MANIFEST_LIMITS.contributionIdMaxLength} chars`);
    return;
  }
  if (seen.has(id)) errors.push(`${where}: duplicate id "${id}"`);
  seen.add(id);
}

function checkButton(errors: string[], where: string, button: unknown, seen: Set<string>): void {
  if (!isRecord(button)) {
    errors.push(`${where}: must be an object`);
    return;
  }
  checkContributionId(errors, where, button.id, seen);
  if (typeof button.label !== 'string' || button.label.length === 0 || button.label.length > MANIFEST_LIMITS.buttonLabelMaxLength) {
    errors.push(`${where}: label must be 1..${MANIFEST_LIMITS.buttonLabelMaxLength} chars (UI text must never truncate)`);
  }
  if (typeof button.icon !== 'string' || !ICON_RE.test(button.icon)) {
    errors.push(`${where}: icon must be a lowercase icon name`);
  }
}

/**
 * Validate an untrusted manifest object. Pure. Errors are plain sentences
 * suitable for the Settings "could not load" state; warnings are for logs.
 */
export function validateManifest(input: unknown): ManifestValidation {
  const errors: string[] = [];
  const warnings: string[] = [];
  if (!isRecord(input)) return { ok: false, errors: ['manifest must be a JSON object'] };

  if (input.manifestVersion !== MANIFEST_VERSION) errors.push(`manifestVersion must be ${MANIFEST_VERSION}`);
  if (!isPluginId(input.id)) {
    errors.push(`id must be publisher-prefixed like "acme.my-plugin" (lowercase, dot-separated, at most ${MANIFEST_LIMITS.idMaxLength} chars)`);
  }
  if (!isReleaseVersion(input.version)) errors.push('version must be strict major.minor.patch');
  if (typeof input.name !== 'string' || input.name.trim().length === 0 || input.name.length > MANIFEST_LIMITS.nameMaxLength) {
    errors.push(`name must be 1..${MANIFEST_LIMITS.nameMaxLength} chars`);
  }
  if (typeof input.description !== 'string' || input.description.length > MANIFEST_LIMITS.descriptionMaxLength) {
    errors.push(`description must be a string of at most ${MANIFEST_LIMITS.descriptionMaxLength} chars`);
  }
  if (typeof input.apiVersion !== 'number' || !Number.isInteger(input.apiVersion) || input.apiVersion < 1) {
    errors.push('apiVersion must be a positive integer');
  }
  if (!isReleaseVersion(input.minHostVersion)) errors.push('minHostVersion must be strict major.minor.patch');
  const scope = input.scope;
  if (scope !== 'meeting' && scope !== 'local') errors.push('scope must be "meeting" or "local"');
  if (typeof input.entry !== 'string' || !ENTRY_RE.test(input.entry)) {
    errors.push('entry must be a bare .js filename with no path separators');
  }

  const permissions: Permission[] = [];
  if (!Array.isArray(input.permissions)) {
    errors.push('permissions must be an array');
  } else {
    const seen = new Set<string>();
    for (const raw of input.permissions) {
      if (typeof raw !== 'string') {
        errors.push('permissions must be strings');
        continue;
      }
      if (seen.has(raw)) {
        errors.push(`duplicate permission "${raw}"`);
        continue;
      }
      seen.add(raw);
      if ((RESERVED_PERMISSIONS as readonly string[]).includes(raw)) {
        errors.push(`permission "${raw}" is not supported by this host`);
        continue;
      }
      if (raw === 'net:fetch:*') {
        errors.push('net:fetch:* is not allowed; list hosts or use net:fetch:user-urls');
        continue;
      }
      if (!isPermission(raw)) {
        errors.push(`unknown permission "${raw}"`);
        continue;
      }
      if (scope === 'local' && MEETING_ONLY_PERMISSIONS.includes(raw)) {
        errors.push(`permission "${raw}" requires scope "meeting"`);
        continue;
      }
      permissions.push(raw);
    }
  }
  const has = (p: Permission) => permissions.includes(p);

  const contributes = input.contributes;
  if (contributes !== undefined) {
    if (!isRecord(contributes)) {
      errors.push('contributes must be an object');
    } else {
      const declaredSurfaces = new Set<string>();
      const surfaces = contributes.surfaces;
      if (surfaces !== undefined) {
        if (!isRecord(surfaces)) {
          errors.push('contributes.surfaces must be an object');
        } else {
          for (const [kind, spec] of Object.entries(surfaces)) {
            if (!(SURFACE_KINDS as readonly string[]).includes(kind)) {
              errors.push(`contributes.surfaces.${kind}: unknown surface kind`);
              continue;
            }
            if (spec === null || spec === undefined) continue;
            if (!isRecord(spec)) {
              errors.push(`contributes.surfaces.${kind}: must be an object or null`);
              continue;
            }
            checkContributionId(errors, `contributes.surfaces.${kind}`, spec.id, new Set());
            if (!has(SURFACE_PERMISSION[kind as SurfaceKind])) {
              errors.push(`contributes.surfaces.${kind} requires permission "${SURFACE_PERMISSION[kind as SurfaceKind]}"`);
            }
            for (const dim of ['width', 'height'] as const) {
              if (spec[dim] !== undefined && (typeof spec[dim] !== 'number' || !(spec[dim] as number > 0))) {
                errors.push(`contributes.surfaces.${kind}.${dim} must be a positive number`);
              }
            }
            if (typeof spec.id === 'string') declaredSurfaces.add(`${kind}:${spec.id}`);
          }
        }
      }

      const toolbar = contributes.toolbarButtons;
      if (toolbar !== undefined) {
        if (!Array.isArray(toolbar)) {
          errors.push('contributes.toolbarButtons must be an array');
        } else {
          if (toolbar.length > 0 && !has('ui:toolbar-button')) errors.push('contributes.toolbarButtons requires permission "ui:toolbar-button"');
          const seen = new Set<string>();
          toolbar.forEach((button, i) => {
            const where = `contributes.toolbarButtons[${i}]`;
            checkButton(errors, where, button, seen);
            if (isRecord(button) && button.opens !== undefined) {
              if (typeof button.opens !== 'string' || !declaredSurfaces.has(button.opens)) {
                errors.push(`${where}.opens must name a declared surface as "<kind>:<id>"`);
              }
            }
          });
        }
      }

      const header = contributes.headerButtons;
      if (header !== undefined) {
        if (!Array.isArray(header)) {
          errors.push('contributes.headerButtons must be an array');
        } else {
          if (header.length > 0 && !has('ui:header-button')) errors.push('contributes.headerButtons requires permission "ui:header-button"');
          const seen = new Set<string>();
          header.forEach((button, i) => checkButton(errors, `contributes.headerButtons[${i}]`, button, seen));
        }
      }

      const settings = contributes.settings;
      if (settings !== undefined) {
        if (!Array.isArray(settings)) {
          errors.push('contributes.settings must be an array');
        } else {
          if (settings.length > 0 && !has('ui:settings')) errors.push('contributes.settings requires permission "ui:settings"');
          const seen = new Set<string>();
          settings.forEach((field, i) => {
            const where = `contributes.settings[${i}]`;
            if (!isRecord(field)) {
              errors.push(`${where}: must be an object`);
              return;
            }
            checkContributionId(errors, where, field.key, seen);
            if (field.type !== 'text' && field.type !== 'url' && field.type !== 'boolean') {
              errors.push(`${where}.type must be "text", "url", or "boolean"`);
            }
            if (typeof field.label !== 'string' || field.label.length === 0 || field.label.length > 40) {
              errors.push(`${where}.label must be 1..40 chars`);
            }
            if (field.netAllow !== undefined) {
              if (field.type !== 'url') errors.push(`${where}.netAllow is only valid on "url" fields`);
              if (field.netAllow === true && !has('net:fetch:user-urls')) {
                errors.push(`${where}.netAllow requires permission "net:fetch:user-urls"`);
              }
            }
          });
        }
      }
    }
  }

  const native = input.native;
  if (native !== undefined && native !== null) {
    if (!isRecord(native) || typeof native.wasm !== 'string' || typeof native.abi !== 'string') {
      errors.push('native must be null or { wasm: string, abi: string, capabilities?: string[] }');
    } else {
      warnings.push('native tier is not supported by this host; the native slot is ignored');
    }
  }

  if (errors.length > 0) return { ok: false, errors };

  const manifest: PluginManifest = {
    manifestVersion: MANIFEST_VERSION,
    id: input.id as string,
    version: input.version as string,
    name: (input.name as string).trim(),
    description: input.description as string,
    apiVersion: input.apiVersion as number,
    minHostVersion: input.minHostVersion as string,
    scope: scope as PluginScope,
    entry: input.entry as string,
    permissions,
  };
  if (contributes !== undefined) manifest.contributes = contributes as PluginContributions;
  if (native !== undefined) manifest.native = native as NativeSlot | null;
  return { ok: true, manifest, warnings };
}

export type HostCompatibility = { ok: true } | { ok: false; reason: string };

/** Can this host (by app version and API major) run the plugin at all? */
export function hostCompatibility(
  manifest: Pick<PluginManifest, 'apiVersion' | 'minHostVersion'>,
  hostVersion: string,
  hostApiVersion: number = HOST_API_VERSION,
): HostCompatibility {
  if (manifest.apiVersion > hostApiVersion) {
    return { ok: false, reason: `needs plugin API ${manifest.apiVersion}; this Petal supports ${hostApiVersion}` };
  }
  const host = releaseCore(hostVersion);
  if (host === null) return { ok: false, reason: `host version "${hostVersion}" is not a release version` };
  if (compareVersions(host, manifest.minHostVersion) < 0) {
    return { ok: false, reason: `needs Petal ${manifest.minHostVersion} or newer; this is ${host}` };
  }
  return { ok: true };
}

/** `0.9.7-dev+abc` -> `0.9.7`; null when there is no release core. */
export function releaseCore(version: string): string | null {
  const core = version.split(/[-+]/, 1)[0] ?? '';
  return isReleaseVersion(core) ? core : null;
}
