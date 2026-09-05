// The API a plugin sees. This is the contract plugin AUTHORS program against
// (re-exported by `@petal/plugin-sdk`) and the shape the frame runtime in
// frameRuntime.ts implements. Host-side code never imports the runtime; both
// sides agree on the bridge methods/events in protocol.ts.
// Design: plugins/README.md §2.4.

import type { Permission, PluginScope, SurfaceKind } from './manifest.ts';

export type MeetingPhase = 'connecting' | 'connected' | 'disconnected';

export interface Participant {
  identity: string;
  name: string;
  isLocal: boolean;
  speaking: boolean;
  micMuted: boolean;
}

export interface RoomInfo {
  label: string;
  phase: MeetingPhase;
}

export interface SharedWindow {
  ownerIdentity: string;
  windowId: string;
  title: string;
  sourceUrl: string | null;
  kind: string;
}

export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

export type Unsubscribe = () => void;

export interface DataMessage {
  sub: string | null;
  /** Always the authenticated LiveKit sender; never read from the payload. */
  sender: Participant;
  payload: Uint8Array;
  json<T = unknown>(): T;
}

export interface PublishOptions {
  reliable?: boolean;
  /** Identities to deliver to; omitted = everyone in the meeting. */
  to?: string[];
}

export interface UiAction {
  source: 'toolbar' | 'header';
  buttonId: string;
  context?: { ownerIdentity: string; windowId: string };
}

export interface ButtonPatch {
  label?: string;
  icon?: string;
  badge?: number | null;
  disabled?: boolean;
}

/** A logic<->surface message port that buffers until the surface mounts. */
export interface SurfaceChannel {
  postMessage(message: Json): void;
  onMessage(cb: (message: Json) => void): Unsubscribe;
}

export interface FetchInit {
  method?: 'GET' | 'POST' | 'PUT' | 'DELETE' | 'PATCH';
  headers?: Record<string, string>;
  body?: string;
}

export interface FetchResult {
  status: number;
  ok: boolean;
  headers: Record<string, string>;
  text(): string;
  json<T = unknown>(): T;
}

export type LogLevel = 'debug' | 'info' | 'warn' | 'error';

export interface HostSupports {
  native: boolean;
  frames: boolean;
}

export interface Petal {
  readonly apiVersion: number;
  readonly hostVersion: string;
  readonly hostSupports: HostSupports;
  readonly plugin: {
    id: string;
    version: string;
    scope: PluginScope;
    permissions: readonly Permission[];
  };
  meeting: {
    self(): Participant | null;
    participants(): Participant[];
    room(): RoomInfo;
    on(event: 'participant-joined' | 'participant-left' | 'participant-changed', cb: (p: Participant) => void): Unsubscribe;
    on(event: 'phase', cb: (room: RoomInfo) => void): Unsubscribe;
  };
  data: {
    publish(sub: string | null, payload: Uint8Array | Json, opts?: PublishOptions): Promise<void>;
    on(sub: string | null, cb: (message: DataMessage) => void): Unsubscribe;
  };
  state: {
    set(value: Json | null): Promise<void>;
    get(identity: string): Json | undefined;
    on(cb: (identity: string, value: Json | undefined) => void): Unsubscribe;
  };
  storage: {
    get<T extends Json = Json>(key: string): Promise<T | undefined>;
    set(key: string, value: Json): Promise<void>;
    delete(key: string): Promise<void>;
    keys(): Promise<string[]>;
  };
  ui: {
    channel(surfaceId: string): SurfaceChannel;
    onAction(cb: (action: UiAction) => void): Unsubscribe;
    setButton(buttonId: string, patch: ButtonPatch): Promise<void>;
    openSurface(surfaceId: string): Promise<void>;
    closeSurface(surfaceId: string): Promise<void>;
    toast(text: string, opts?: { variant?: 'info' | 'degraded' }): Promise<void>;
  };
  shares: {
    list(): SharedWindow[];
    on(cb: (shares: SharedWindow[]) => void): Unsubscribe;
  };
  net: {
    fetch(url: string, init?: FetchInit): Promise<FetchResult>;
  };
  clipboard: {
    writeText(text: string): Promise<void>;
  };
  log: Record<LogLevel, (...args: unknown[]) => void>;
}

export interface SurfaceContext {
  id: string;
  kind: SurfaceKind;
  root: HTMLElement;
  channel: SurfaceChannel;
}

export interface PluginDefinition {
  activate?(petal: Petal): void | Promise<void>;
  mountSurface?(petal: Petal, surface: SurfaceContext): void | Promise<void>;
}
