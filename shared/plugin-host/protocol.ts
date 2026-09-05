// The postMessage envelope between the host page and a plugin frame. Pinned
// in contracts/petal-contracts.json (`pluginBridge`, M2). Both directions use
// the same three envelope kinds; the frame only ever sends `req` and a few
// lifecycle `evt`s, the host only ever sends `res` and `evt`.
// Design: plugins/README.md §2.3.

import type { Participant, RoomInfo, SharedWindow, Json, HostSupports } from './api.ts';
import type { Permission, PluginScope, SurfaceKind } from './manifest.ts';

export const PROTOCOL_VERSION = 1 as const;

export type BridgeErrorCode = 'denied' | 'rate-limited' | 'invalid' | 'unavailable' | 'internal';

export const BRIDGE_METHODS = [
  'data.publish',
  'state.set',
  'storage.get',
  'storage.set',
  'storage.delete',
  'storage.keys',
  'ui.setButton',
  'ui.openSurface',
  'ui.closeSurface',
  'ui.toast',
  'net.fetch',
  'clipboard.writeText',
  'log',
] as const;
export type BridgeMethod = (typeof BRIDGE_METHODS)[number];

export function isBridgeMethod(value: unknown): value is BridgeMethod {
  return typeof value === 'string' && (BRIDGE_METHODS as readonly string[]).includes(value);
}

/** Host -> frame events. `init` is always first; the rest are permission-gated. */
export const HOST_EVENTS = [
  'init',
  'meeting.participant-joined',
  'meeting.participant-left',
  'meeting.participant-changed',
  'meeting.phase',
  'data.message',
  'state.changed',
  'shares.changed',
  'ui.action',
  'ui.surface-opened',
  'ui.surface-closed',
] as const;
export type HostEvent = (typeof HOST_EVENTS)[number];

/** Frame -> host lifecycle events. */
export const FRAME_EVENTS = ['ready', 'activated', 'error'] as const;
export type FrameEvent = (typeof FRAME_EVENTS)[number];

export interface RequestEnvelope {
  v: typeof PROTOCOL_VERSION;
  kind: 'req';
  id: number;
  method: string;
  params?: unknown;
}
export interface ResponseOkEnvelope {
  v: typeof PROTOCOL_VERSION;
  kind: 'res';
  id: number;
  ok: true;
  result?: unknown;
}
export interface ResponseErrEnvelope {
  v: typeof PROTOCOL_VERSION;
  kind: 'res';
  id: number;
  ok: false;
  error: { code: BridgeErrorCode; message: string };
}
export interface EventEnvelope {
  v: typeof PROTOCOL_VERSION;
  kind: 'evt';
  event: string;
  payload?: unknown;
}
export type Envelope = RequestEnvelope | ResponseOkEnvelope | ResponseErrEnvelope | EventEnvelope;

export function isEnvelope(value: unknown): value is Envelope {
  if (typeof value !== 'object' || value === null) return false;
  const env = value as Record<string, unknown>;
  if (env.v !== PROTOCOL_VERSION) return false;
  switch (env.kind) {
    case 'req':
      return typeof env.id === 'number' && Number.isInteger(env.id) && typeof env.method === 'string';
    case 'res':
      return typeof env.id === 'number' && typeof env.ok === 'boolean';
    case 'evt':
      return typeof env.event === 'string';
    default:
      return false;
  }
}

export interface InitPayload {
  pluginId: string;
  version: string;
  apiVersion: number;
  scope: PluginScope;
  grantedPermissions: Permission[];
  hostVersion: string;
  hostSupports: HostSupports;
  /** Present only with `meeting:read`. */
  meeting: { self: Participant | null; participants: Participant[]; room: RoomInfo } | null;
  /** Other participants' state for this plugin, by identity. Present only with `meeting:read`. */
  state: Record<string, Json> | null;
  /** Present only with `shares:read`. */
  shares: SharedWindow[] | null;
  /** Set when this frame is a UI surface rather than the plugin's logic frame. */
  surface: { id: string; kind: SurfaceKind } | null;
}

export interface DataMessagePayload {
  sub: string | null;
  sender: Participant;
  payload: Uint8Array;
}

export interface PublishParams {
  sub: string | null;
  payload: Uint8Array;
  reliable: boolean;
  to?: string[];
}

export interface FetchParams {
  url: string;
  method: string;
  headers: Record<string, string>;
  body?: string;
}

export interface FetchResponse {
  status: number;
  headers: Record<string, string>;
  body: string;
}

export function okResponse(id: number, result?: unknown): ResponseOkEnvelope {
  return { v: PROTOCOL_VERSION, kind: 'res', id, ok: true, result };
}

export function errResponse(id: number, code: BridgeErrorCode, message: string): ResponseErrEnvelope {
  return { v: PROTOCOL_VERSION, kind: 'res', id, ok: false, error: { code, message } };
}

export function eventEnvelope(event: HostEvent | FrameEvent, payload?: unknown): EventEnvelope {
  return { v: PROTOCOL_VERSION, kind: 'evt', event, payload };
}
