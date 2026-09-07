// Which permission each bridge method and host event needs, and the
// net-fetch host matching rule. The broker consults this before touching an
// adapter; Rust re-checks `plugin_net_fetch` and `plugin_publish_data`
// independently. Design: plugins/README.md §2.8.

import type { Permission, PluginManifest } from './manifest.ts';
import type { BridgeMethod, HostEvent } from './protocol.ts';

/** `null` = no permission needed. `'net'` = resolved per URL by `netFetchAllowed`. */
export const METHOD_PERMISSIONS: Record<BridgeMethod, Permission | 'net' | null> = {
  'data.publish': 'data:publish',
  'state.set': 'state:write',
  'storage.get': 'storage',
  'storage.set': 'storage',
  'storage.delete': 'storage',
  'storage.keys': 'storage',
  'ui.setButton': null, // scoped to the plugin's own declared buttons by the adapter
  'ui.openSurface': null, // likewise: only declared surfaces exist to open
  'ui.closeSurface': null,
  'ui.toast': 'ui:toast',
  'net.fetch': 'net',
  'clipboard.writeText': 'clipboard:write',
  log: null,
};

/** Events the host only forwards when the plugin holds the permission. */
export const EVENT_PERMISSIONS: Record<HostEvent, Permission | null> = {
  init: null,
  'meeting.participant-joined': 'meeting:read',
  'meeting.participant-left': 'meeting:read',
  'meeting.participant-changed': 'meeting:read',
  'meeting.phase': 'meeting:read',
  'data.message': 'data:publish',
  'state.changed': 'meeting:read',
  'shares.changed': 'shares:read',
  'ui.action': null,
  'ui.surface-opened': null,
  'ui.surface-closed': null,
};

export function hasPermission(granted: readonly Permission[], permission: Permission): boolean {
  return granted.includes(permission);
}

/** `*.example.com` matches `a.example.com` and `a.b.example.com`, never `example.com`. */
export function hostMatches(pattern: string, host: string): boolean {
  const p = pattern.toLowerCase();
  const h = host.toLowerCase();
  if (p.startsWith('*.')) {
    const suffix = p.slice(1); // ".example.com"
    return h.endsWith(suffix) && h.length > suffix.length;
  }
  return p === h;
}

export interface NetContext {
  granted: readonly Permission[];
  /** Origins (`https://hooks.example.com`) collected from the plugin's `url` settings fields. */
  userOrigins?: readonly string[];
}

export type NetDecision = { ok: true; url: URL } | { ok: false; reason: string };

/**
 * A plugin may fetch a URL when (a) it is https (http only for localhost),
 * and (b) the host matches a granted `net:fetch:<host>`, or the plugin holds
 * `net:fetch:user-urls` and the URL's origin was entered by the user.
 */
export function netFetchAllowed(ctx: NetContext, rawUrl: string): NetDecision {
  let url: URL;
  try {
    url = new URL(rawUrl);
  } catch {
    return { ok: false, reason: 'invalid URL' };
  }
  const isLocalhost = url.hostname === 'localhost' || url.hostname === '127.0.0.1' || url.hostname === '[::1]';
  if (url.protocol !== 'https:' && !(url.protocol === 'http:' && isLocalhost)) {
    return { ok: false, reason: 'only https URLs are allowed (http for localhost)' };
  }
  if (url.username || url.password) return { ok: false, reason: 'credentials in URLs are not allowed' };
  const hostWithPort = url.port ? `${url.hostname}:${url.port}` : url.hostname;
  for (const permission of ctx.granted) {
    if (!permission.startsWith('net:fetch:') || permission === 'net:fetch:user-urls') continue;
    const pattern = permission.slice('net:fetch:'.length);
    if (hostMatches(pattern, hostWithPort) || hostMatches(pattern, url.hostname)) return { ok: true, url };
  }
  if (ctx.granted.includes('net:fetch:user-urls') && ctx.userOrigins?.includes(url.origin)) {
    return { ok: true, url };
  }
  return { ok: false, reason: `host "${url.hostname}" is not in this plugin's allowlist` };
}

/** Manifest permissions the user must consent to. Currently: all of them. */
export function permissionsToGrant(manifest: PluginManifest): Permission[] {
  return [...manifest.permissions];
}

/** New permissions a version bump introduces (re-consent trigger). */
export function newPermissions(previous: readonly Permission[], next: readonly Permission[]): Permission[] {
  return next.filter((p) => !previous.includes(p));
}
