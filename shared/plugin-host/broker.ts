// Host-side plugin broker: owns every attached plugin frame, validates each
// envelope, enforces permissions (permissions.ts) and quotas (rateLimit.ts),
// then delegates to a per-client `HostAdapter`. The broker runs in the
// trusted host page and is therefore full-privilege -- keep it small and
// test the denial paths first. Design: plugins/README.md §2.3.

import type { Json, Participant, RoomInfo, SharedWindow, ButtonPatch, LogLevel } from './api.ts';
import type { Permission, PluginManifest, SurfaceKind } from './manifest.ts';
import { HOST_API_VERSION } from './manifest.ts';
import { EVENT_PERMISSIONS, METHOD_PERMISSIONS, hasPermission, netFetchAllowed } from './permissions.ts';
import {
  type BridgeErrorCode,
  type Envelope,
  type FetchParams,
  type FetchResponse,
  type HostEvent,
  type InitPayload,
  type PublishParams,
  type RequestEnvelope,
  errResponse,
  eventEnvelope,
  isBridgeMethod,
  isEnvelope,
  okResponse,
} from './protocol.ts';
import { PLUGIN_LIMITS, createRateLimiter, jsonByteLength, type RateLimiter } from './rateLimit.ts';

export type PluginSource = 'builtin' | 'registry' | 'dev';

export interface LoadedPlugin {
  manifest: PluginManifest;
  granted: readonly Permission[];
  source: PluginSource;
  /** Origins from the plugin's user-entered `url` settings (net:fetch:user-urls). */
  userOrigins?: readonly string[];
}

/** The subset of `Window` the broker needs; lets tests use fakes. */
export interface FrameWindow {
  postMessage(message: unknown, targetOrigin: string, transfer?: Transferable[]): void;
}

export interface HostAdapter {
  meeting?: {
    self(): Participant | null;
    participants(): Participant[];
    room(): RoomInfo;
  };
  /** Other participants' `plugins[<id>].state`, by identity. */
  stateSnapshot?(pluginId: string): Record<string, Json>;
  shares?(): SharedWindow[];
  publishData(plugin: LoadedPlugin, params: PublishParams): Promise<void>;
  setState(plugin: LoadedPlugin, value: Json | null): Promise<void>;
  storage: {
    get(pluginId: string, key: string): Promise<Json | undefined>;
    set(pluginId: string, key: string, value: Json): Promise<void>;
    delete(pluginId: string, key: string): Promise<void>;
    keys(pluginId: string): Promise<string[]>;
  };
  ui: {
    setButton(pluginId: string, buttonId: string, patch: ButtonPatch): void;
    openSurface(pluginId: string, surfaceId: string): void;
    closeSurface(pluginId: string, surfaceId: string): void;
    toast(pluginId: string, text: string, variant: 'info' | 'degraded'): void;
  };
  fetch(plugin: LoadedPlugin, params: FetchParams): Promise<FetchResponse>;
  clipboardWriteText(text: string): Promise<void>;
  log(pluginId: string, level: LogLevel, args: string[]): void;
  /** Lifecycle observations (frame reported ready/activated/error). */
  onFrameEvent?(pluginId: string, event: 'ready' | 'activated' | 'error', payload: unknown): void;
}

export interface AttachOptions {
  /** Present when the frame is a UI surface, not the plugin's logic frame. */
  surface?: { id: string; kind: SurfaceKind };
  /** Surface frames receive their end of the logic<->surface channel with `init`. */
  port?: MessagePort;
}

export interface PluginInstance {
  plugin: LoadedPlugin;
  frame: FrameWindow;
  surface: { id: string; kind: SurfaceKind } | null;
}

export interface PluginBroker {
  attach(plugin: LoadedPlugin, frame: FrameWindow, opts?: AttachOptions): PluginInstance;
  detach(frame: FrameWindow): void;
  detachPlugin(pluginId: string): void;
  /** Route a `message` event from the host window. Returns true when it came from an attached frame. */
  handleMessage(event: { source: unknown; data: unknown }): boolean;
  /** Send a permission-gated event to every frame of one plugin. */
  emit(pluginId: string, event: HostEvent, payload?: unknown, transfer?: Transferable[]): void;
  /** Send a permission-gated event to every attached plugin. */
  broadcast(event: HostEvent, payload?: unknown): void;
  /** Deliver an inbound data packet already parsed to `plugin/<id>[/<sub>]`. */
  deliverData(pluginId: string, message: { sub: string | null; sender: Participant; payload: Uint8Array }): void;
  instances(): PluginInstance[];
  pluginIds(): string[];
}

export interface BrokerOptions {
  adapter: HostAdapter;
  hostVersion: string;
  now?: () => number;
  /** Diagnostics sink for host-side problems (denials, bad envelopes). */
  warn?: (message: string) => void;
}

const SUB_RE = /^[a-z0-9][a-z0-9-]{0,31}$/;
const KEY_RE = /^[A-Za-z0-9_.:-]{1,64}$/;
const CONTRIB_ID_RE = /^[a-z0-9][a-z0-9-]{0,31}$/;

class BridgeError extends Error {
  readonly code: BridgeErrorCode;
  constructor(code: BridgeErrorCode, message: string) {
    super(message);
    this.code = code;
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function expectString(params: Record<string, unknown>, key: string, re?: RegExp): string {
  const value = params[key];
  if (typeof value !== 'string' || (re && !re.test(value))) throw new BridgeError('invalid', `${key} is missing or malformed`);
  return value;
}

export function createPluginBroker({ adapter, hostVersion, now = () => Date.now(), warn = () => {} }: BrokerOptions): PluginBroker {
  const byFrame = new Map<FrameWindow, PluginInstance>();
  const deniedOnce = new Set<string>();
  const limiters: Record<string, RateLimiter> = {
    lossy: createRateLimiter({ perSecond: PLUGIN_LIMITS.lossyPerSecond, now }),
    reliable: createRateLimiter({ perSecond: PLUGIN_LIMITS.reliablePerSecond, now }),
    state: createRateLimiter({ perSecond: PLUGIN_LIMITS.statePerSecond, now }),
    storage: createRateLimiter({ perSecond: PLUGIN_LIMITS.storageWritesPerSecond, now }),
    toast: createRateLimiter({ perSecond: PLUGIN_LIMITS.toastPerSecond, burst: 1, now }),
    net: createRateLimiter({ perSecond: PLUGIN_LIMITS.netFetchPerSecond, now }),
    log: createRateLimiter({ perSecond: PLUGIN_LIMITS.logPerSecond, now }),
    ui: createRateLimiter({ perSecond: PLUGIN_LIMITS.uiPerSecond, now }),
  };

  function take(bucket: keyof typeof limiters, pluginId: string): void {
    if (!limiters[bucket]!.tryTake(pluginId)) throw new BridgeError('rate-limited', `${bucket} quota exceeded`);
  }

  function post(frame: FrameWindow, env: Envelope, transfer?: Transferable[]): void {
    try {
      // Sandboxed frames have an opaque origin; '*' is the only target that reaches them.
      frame.postMessage(env, '*', transfer);
    } catch (e) {
      warn(`plugin broker: postMessage failed: ${String(e)}`);
    }
  }

  function initPayload(plugin: LoadedPlugin, surface: AttachOptions['surface']): InitPayload {
    const m = plugin.manifest;
    const canRead = hasPermission(plugin.granted, 'meeting:read');
    return {
      pluginId: m.id,
      version: m.version,
      apiVersion: HOST_API_VERSION,
      scope: m.scope,
      grantedPermissions: [...plugin.granted],
      hostVersion,
      hostSupports: { native: false, frames: false },
      meeting:
        canRead && adapter.meeting
          ? { self: adapter.meeting.self(), participants: adapter.meeting.participants(), room: adapter.meeting.room() }
          : null,
      state: canRead && adapter.stateSnapshot ? adapter.stateSnapshot(m.id) : null,
      shares: hasPermission(plugin.granted, 'shares:read') && adapter.shares ? adapter.shares() : null,
      surface: surface ?? null,
    };
  }

  function declaredButton(plugin: LoadedPlugin, buttonId: string): boolean {
    const c = plugin.manifest.contributes;
    return Boolean(c?.toolbarButtons?.some((b) => b.id === buttonId) || c?.headerButtons?.some((b) => b.id === buttonId));
  }

  function declaredSurface(plugin: LoadedPlugin, surfaceId: string): boolean {
    const surfaces = plugin.manifest.contributes?.surfaces;
    if (!surfaces) return false;
    return Object.values(surfaces).some((s) => s && s.id === surfaceId);
  }

  async function dispatch(instance: PluginInstance, req: RequestEnvelope): Promise<unknown> {
    const { plugin } = instance;
    const id = plugin.manifest.id;
    if (!isBridgeMethod(req.method)) throw new BridgeError('invalid', `unknown method "${req.method}"`);
    const method = req.method;
    const params = isRecord(req.params) ? req.params : {};

    const needed = METHOD_PERMISSIONS[method];
    if (needed !== null && needed !== 'net' && !hasPermission(plugin.granted, needed)) {
      throw new BridgeError('denied', `"${method}" requires permission "${needed}"`);
    }

    switch (method) {
      case 'data.publish': {
        const payload = params.payload;
        if (!(payload instanceof Uint8Array)) throw new BridgeError('invalid', 'payload must be bytes');
        if (payload.byteLength > PLUGIN_LIMITS.maxPayloadBytes) {
          throw new BridgeError('invalid', `payload exceeds ${PLUGIN_LIMITS.maxPayloadBytes} bytes`);
        }
        const sub = params.sub === null || params.sub === undefined ? null : expectString(params, 'sub', SUB_RE);
        const reliable = params.reliable !== false;
        let to: string[] | undefined;
        if (params.to !== undefined) {
          if (!Array.isArray(params.to) || params.to.some((t) => typeof t !== 'string') || params.to.length > 64) {
            throw new BridgeError('invalid', 'to must be a list of identities');
          }
          to = params.to as string[];
        }
        take(reliable ? 'reliable' : 'lossy', id);
        await adapter.publishData(plugin, { sub, payload, reliable, to });
        return undefined;
      }
      case 'state.set': {
        const value = (params.value ?? null) as Json | null;
        if (jsonByteLength(value) > PLUGIN_LIMITS.stateMaxBytes) {
          throw new BridgeError('invalid', `state exceeds ${PLUGIN_LIMITS.stateMaxBytes} bytes`);
        }
        take('state', id);
        await adapter.setState(plugin, value);
        return undefined;
      }
      case 'storage.get':
        return adapter.storage.get(id, expectString(params, 'key', KEY_RE));
      case 'storage.set': {
        const key = expectString(params, 'key', KEY_RE);
        if (jsonByteLength(params.value) > PLUGIN_LIMITS.storageValueMaxBytes) {
          throw new BridgeError('invalid', `value exceeds ${PLUGIN_LIMITS.storageValueMaxBytes} bytes`);
        }
        take('storage', id);
        await adapter.storage.set(id, key, params.value as Json);
        return undefined;
      }
      case 'storage.delete':
        take('storage', id);
        await adapter.storage.delete(id, expectString(params, 'key', KEY_RE));
        return undefined;
      case 'storage.keys':
        return adapter.storage.keys(id);
      case 'ui.setButton': {
        const buttonId = expectString(params, 'buttonId', CONTRIB_ID_RE);
        if (!declaredButton(plugin, buttonId)) throw new BridgeError('invalid', `no declared button "${buttonId}"`);
        const patch = isRecord(params.patch) ? params.patch : {};
        const clean: ButtonPatch = {};
        if (typeof patch.label === 'string') clean.label = patch.label.slice(0, 14);
        if (typeof patch.icon === 'string') clean.icon = patch.icon;
        if (patch.badge === null || (typeof patch.badge === 'number' && Number.isFinite(patch.badge))) clean.badge = patch.badge;
        if (typeof patch.disabled === 'boolean') clean.disabled = patch.disabled;
        take('ui', id);
        adapter.ui.setButton(id, buttonId, clean);
        return undefined;
      }
      case 'ui.openSurface':
      case 'ui.closeSurface': {
        const surfaceId = expectString(params, 'surfaceId', CONTRIB_ID_RE);
        if (!declaredSurface(plugin, surfaceId)) throw new BridgeError('invalid', `no declared surface "${surfaceId}"`);
        take('ui', id);
        if (method === 'ui.openSurface') adapter.ui.openSurface(id, surfaceId);
        else adapter.ui.closeSurface(id, surfaceId);
        return undefined;
      }
      case 'ui.toast': {
        const text = expectString(params, 'text');
        if (text.trim().length === 0 || text.length > PLUGIN_LIMITS.toastMaxChars) {
          throw new BridgeError('invalid', `toast text must be 1..${PLUGIN_LIMITS.toastMaxChars} chars`);
        }
        const variant = params.variant === 'degraded' ? 'degraded' : 'info';
        take('toast', id);
        adapter.ui.toast(id, text, variant);
        return undefined;
      }
      case 'net.fetch': {
        const url = expectString(params, 'url');
        const decision = netFetchAllowed({ granted: plugin.granted, userOrigins: plugin.userOrigins }, url);
        if (!decision.ok) throw new BridgeError('denied', decision.reason);
        const methodName = typeof params.method === 'string' ? params.method.toUpperCase() : 'GET';
        if (!['GET', 'POST', 'PUT', 'DELETE', 'PATCH'].includes(methodName)) throw new BridgeError('invalid', 'unsupported HTTP method');
        const headers: Record<string, string> = {};
        if (isRecord(params.headers)) {
          for (const [k, v] of Object.entries(params.headers)) {
            if (typeof v !== 'string') continue;
            if (/^(cookie|authorization|host|origin|referer)$/i.test(k)) continue; // the plugin holds no ambient credentials
            headers[k] = v;
          }
        }
        const body = typeof params.body === 'string' ? params.body : undefined;
        if (body !== undefined && body.length > PLUGIN_LIMITS.netResponseMaxBytes) throw new BridgeError('invalid', 'body too large');
        take('net', id);
        return adapter.fetch(plugin, { url: decision.url.toString(), method: methodName, headers, body });
      }
      case 'clipboard.writeText': {
        const text = expectString(params, 'text');
        if (text.length > 65536) throw new BridgeError('invalid', 'text too large');
        take('ui', id);
        await adapter.clipboardWriteText(text);
        return undefined;
      }
      case 'log': {
        const level = params.level;
        if (level !== 'debug' && level !== 'info' && level !== 'warn' && level !== 'error') throw new BridgeError('invalid', 'bad level');
        const args = Array.isArray(params.args) ? params.args.map((a) => String(a).slice(0, 2000)).slice(0, 16) : [];
        if (!limiters.log!.tryTake(id)) return undefined; // drop silently; logging must never throw at the plugin
        adapter.log(id, level, args);
        return undefined;
      }
    }
  }

  function handleRequest(instance: PluginInstance, req: RequestEnvelope): void {
    dispatch(instance, req).then(
      (result) => post(instance.frame, okResponse(req.id, result)),
      (e: unknown) => {
        const err = e instanceof BridgeError ? e : new BridgeError('internal', 'host error');
        if (err.code === 'denied') {
          const key = `${instance.plugin.manifest.id}:${req.method}`;
          if (!deniedOnce.has(key)) {
            deniedOnce.add(key);
            warn(`plugin ${instance.plugin.manifest.id}: ${req.method} denied: ${err.message}`);
          }
        } else if (err.code === 'internal') {
          warn(`plugin ${instance.plugin.manifest.id}: ${req.method} failed: ${String(e)}`);
        }
        post(instance.frame, errResponse(req.id, err.code, err.message));
      },
    );
  }

  function sendGated(instance: PluginInstance, event: HostEvent, payload: unknown, transfer?: Transferable[]): void {
    const needed = EVENT_PERMISSIONS[event];
    if (needed !== null && !hasPermission(instance.plugin.granted, needed)) return;
    post(instance.frame, eventEnvelope(event, payload), transfer);
  }

  return {
    attach(plugin, frame, opts = {}) {
      const instance: PluginInstance = { plugin, frame, surface: opts.surface ?? null };
      byFrame.set(frame, instance);
      post(frame, eventEnvelope('init', initPayload(plugin, opts.surface)), opts.port ? [opts.port] : undefined);
      return instance;
    },
    detach(frame) {
      byFrame.delete(frame);
    },
    detachPlugin(pluginId) {
      for (const [frame, inst] of byFrame) if (inst.plugin.manifest.id === pluginId) byFrame.delete(frame);
    },
    handleMessage(event) {
      const instance = byFrame.get(event.source as FrameWindow);
      if (!instance) return false;
      const data = event.data;
      if (!isEnvelope(data)) {
        warn(`plugin ${instance.plugin.manifest.id}: malformed envelope ignored`);
        return true;
      }
      if (data.kind === 'req') {
        handleRequest(instance, data);
      } else if (data.kind === 'evt') {
        if (data.event === 'ready' || data.event === 'activated' || data.event === 'error') {
          adapter.onFrameEvent?.(instance.plugin.manifest.id, data.event, data.payload);
        }
      }
      // 'res' from a frame is meaningless; the host never sends requests.
      return true;
    },
    emit(pluginId, event, payload, transfer) {
      for (const inst of byFrame.values()) if (inst.plugin.manifest.id === pluginId) sendGated(inst, event, payload, transfer);
    },
    broadcast(event, payload) {
      for (const inst of byFrame.values()) sendGated(inst, event, payload);
    },
    deliverData(pluginId, message) {
      // Only the plugin's LOGIC frame handles data; surfaces talk to it over their channel.
      for (const inst of byFrame.values()) {
        if (inst.plugin.manifest.id !== pluginId || inst.surface) continue;
        sendGated(inst, 'data.message', message);
      }
    },
    instances() {
      return [...byFrame.values()];
    },
    pluginIds() {
      return [...new Set([...byFrame.values()].map((i) => i.plugin.manifest.id))];
    },
  };
}
