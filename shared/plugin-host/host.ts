// The plugin host: everything both clients share about RUNNING plugins in a
// page. Creates the sandboxed logic frame per plugin, opens/closes UI surface
// frames (overlay, popover), brokers the logic<->surface MessageChannel,
// keeps the toolbar-button models, and routes window `message` events to the
// broker. The client supplies a `PluginHostAdapter` (meeting snapshot,
// storage, toast, transport) and three mount elements. Design:
// plugins/README.md §2.3 and §2.7.

import type { ButtonPatch, Participant } from './api.ts';
import { createPluginBroker, type HostAdapter, type LoadedPlugin, type PluginBroker } from './broker.ts';
import { createPluginFrame } from './frame.ts';
import type { SurfaceContribution, SurfaceKind } from './manifest.ts';
import type { HostEvent } from './protocol.ts';
import { buttonKey, placePopover, toolbarButtonModels, type ToolbarButtonModel } from './surfaces.ts';
import { installDismissibleLayer, type DismissibleLayerCleanup } from '../ui/dismissibleLayer.ts';

export type PluginHostAdapter = Omit<HostAdapter, 'ui'> & {
  toast(pluginId: string, text: string, variant: 'info' | 'degraded'): void;
};

export interface PluginHostMounts {
  /** Hidden container for logic frames. */
  logic: HTMLElement;
  /** Positioned container covering the meeting content; overlay frames fill it. */
  overlay: HTMLElement;
  /** Fixed, full-viewport layer popovers are placed in. */
  popoverLayer: HTMLElement;
}

export interface PluginHostOptions {
  document: Document;
  adapter: PluginHostAdapter;
  hostVersion: string;
  mounts: PluginHostMounts;
  onButtonsChanged?: (buttons: ToolbarButtonModel[]) => void;
  warn?: (message: string) => void;
  now?: () => number;
}

export interface PluginHost {
  readonly broker: PluginBroker;
  /** Boot a plugin: create its logic frame and (if declared) its overlay. */
  load(plugin: LoadedPlugin, source: string): void;
  unload(pluginId: string): void;
  loaded(): LoadedPlugin[];
  isLoaded(pluginId: string): boolean;
  buttons(): ToolbarButtonModel[];
  /** A host-drawn toolbar button was clicked. */
  activateButton(pluginId: string, buttonId: string, anchor: HTMLElement | null): void;
  openSurface(pluginId: string, surfaceId: string, anchor?: HTMLElement | null): void;
  closeSurface(pluginId: string, surfaceId: string): void;
  /** Convenience passthroughs the client calls from its own event sources. */
  emit(pluginId: string, event: HostEvent, payload?: unknown): void;
  broadcast(event: HostEvent, payload?: unknown): void;
  deliverData(pluginId: string, message: { sub: string | null; sender: Participant; payload: Uint8Array }): void;
  dispose(): void;
}

interface SurfaceInstance {
  kind: SurfaceKind;
  id: string;
  frame: HTMLIFrameElement;
  container: HTMLElement | null;
  cleanup: (() => void)[];
}

interface LoadedEntry {
  plugin: LoadedPlugin;
  source: string;
  frame: HTMLIFrameElement;
  surfaces: Map<string, SurfaceInstance>;
}

const POPOVER_DEFAULT = { width: 280, height: 200 };

export function createPluginHost(opts: PluginHostOptions): PluginHost {
  const { document: doc, adapter, mounts } = opts;
  const warn = opts.warn ?? (() => {});
  const maybeWin = doc.defaultView;
  if (!maybeWin) throw new Error('plugin host needs a document with a window');
  // A separate const so hoisted function declarations below keep the narrowing.
  const win: Window = maybeWin;

  const entries = new Map<string, LoadedEntry>();
  const patches = new Map<string, ButtonPatch>();

  function notifyButtons(): void {
    opts.onButtonsChanged?.(buttons());
  }

  function buttons(): ToolbarButtonModel[] {
    return toolbarButtonModels([...entries.values()].map((e) => e.plugin), patches);
  }

  function declaredSurface(plugin: LoadedPlugin, surfaceId: string): { kind: SurfaceKind; spec: SurfaceContribution } | null {
    const surfaces = plugin.manifest.contributes?.surfaces;
    if (!surfaces) return null;
    for (const [kind, spec] of Object.entries(surfaces)) {
      if (spec && spec.id === surfaceId) return { kind: kind as SurfaceKind, spec };
    }
    return null;
  }

  const broker = createPluginBroker({
    adapter: {
      ...adapter,
      onFrameEvent(pluginId, event, payload) {
        if (event === 'dismiss') {
          const surfaceId = (payload as { surfaceId?: unknown } | undefined)?.surfaceId;
          const surface = typeof surfaceId === 'string' ? entries.get(pluginId)?.surfaces.get(surfaceId) : undefined;
          if (surface?.kind === 'popover') closeSurface(pluginId, surfaceId as string);
        }
        adapter.onFrameEvent?.(pluginId, event, payload);
      },
      ui: {
        setButton(pluginId, buttonId, patch) {
          const key = buttonKey(pluginId, buttonId);
          patches.set(key, { ...patches.get(key), ...patch });
          notifyButtons();
        },
        openSurface(pluginId, surfaceId) {
          openSurface(pluginId, surfaceId, null);
        },
        closeSurface(pluginId, surfaceId) {
          closeSurface(pluginId, surfaceId);
        },
        toast(pluginId, text, variant) {
          adapter.toast(pluginId, text, variant);
        },
      },
    },
    hostVersion: opts.hostVersion,
    now: opts.now,
    warn,
  });

  function onMessage(event: MessageEvent): void {
    broker.handleMessage(event);
  }
  win.addEventListener('message', onMessage);

  /** Attach once the srcdoc has loaded so the runtime's listener exists before `init` is posted. */
  function attachWhenLoaded(frame: HTMLIFrameElement, attach: () => void): () => void {
    let done = false;
    const run = () => {
      if (done) return;
      done = true;
      attach();
    };
    frame.addEventListener('load', run, { once: true });
    return () => {
      done = true;
      frame.removeEventListener('load', run);
    };
  }

  function load(plugin: LoadedPlugin, source: string): void {
    const id = plugin.manifest.id;
    if (entries.has(id)) unload(id);
    const frame = createPluginFrame(doc, { pluginId: id, source, surface: false, className: 'petal-plugin-logic' });
    const entry: LoadedEntry = { plugin, source, frame, surfaces: new Map() };
    entries.set(id, entry);
    attachWhenLoaded(frame, () => {
      if (!frame.contentWindow || entries.get(id) !== entry) return;
      broker.attach(plugin, frame.contentWindow);
      const overlay = plugin.manifest.contributes?.surfaces?.overlay;
      if (overlay && plugin.granted.includes('ui:overlay')) openSurface(id, overlay.id, null);
    });
    mounts.logic.appendChild(frame);
    notifyButtons();
  }

  function unload(pluginId: string): void {
    const entry = entries.get(pluginId);
    if (!entry) return;
    for (const surfaceId of [...entry.surfaces.keys()]) closeSurface(pluginId, surfaceId);
    broker.detachPlugin(pluginId);
    entry.frame.remove();
    entries.delete(pluginId);
    for (const key of [...patches.keys()]) if (key.startsWith(`${pluginId}/`)) patches.delete(key);
    notifyButtons();
  }

  function openSurface(pluginId: string, surfaceId: string, anchor: HTMLElement | null | undefined): void {
    const entry = entries.get(pluginId);
    if (!entry) return;
    const declared = declaredSurface(entry.plugin, surfaceId);
    if (!declared) {
      warn(`plugin ${pluginId}: no declared surface "${surfaceId}"`);
      return;
    }
    if (entry.surfaces.has(surfaceId)) {
      if (declared.kind === 'popover') closeSurface(pluginId, surfaceId); // toggle
      return;
    }
    if (declared.kind !== 'overlay' && declared.kind !== 'popover') {
      throw Object.assign(new Error(`surface kind "${declared.kind}" is not available on this host yet`), { code: 'unavailable' });
    }
    // Popovers are exclusive: opening one closes any other plugin's popover.
    if (declared.kind === 'popover') {
      for (const other of entries.values()) {
        for (const [sid, s] of other.surfaces) if (s.kind === 'popover') closeSurface(other.plugin.manifest.id, sid);
      }
    }

    const channel = new MessageChannel();
    const frame = createPluginFrame(doc, {
      pluginId,
      source: entry.source,
      surface: true,
      className: `petal-plugin-surface petal-plugin-surface-${declared.kind}`,
      title: `${entry.plugin.manifest.name} ${declared.kind}`,
    });
    frame.setAttribute('allowtransparency', 'true');
    frame.style.border = '0';
    frame.style.background = 'transparent';
    frame.style.colorScheme = 'dark';

    const instance: SurfaceInstance = { kind: declared.kind, id: surfaceId, frame, container: null, cleanup: [] };
    entry.surfaces.set(surfaceId, instance);

    const detachLoad = attachWhenLoaded(frame, () => {
      if (!frame.contentWindow || entry.surfaces.get(surfaceId) !== instance) return;
      broker.attach(entry.plugin, frame.contentWindow, { surface: { id: surfaceId, kind: declared.kind }, port: channel.port2 });
      broker.emitLogic(pluginId, 'ui.surface-opened', { surfaceId, kind: declared.kind }, [channel.port1]);
    });
    instance.cleanup.push(detachLoad);

    if (declared.kind === 'overlay') {
      frame.style.position = 'absolute';
      frame.style.inset = '0';
      frame.style.width = '100%';
      frame.style.height = '100%';
      frame.style.pointerEvents = 'none';
      mounts.overlay.appendChild(frame);
      return;
    }

    // Popover: fixed container placed against the anchor, dismissed by an outside pointerdown.
    const container = doc.createElement('div');
    container.className = 'petal-plugin-popover';
    container.setAttribute('role', 'dialog');
    container.setAttribute('aria-label', `${entry.plugin.manifest.name}`);
    container.style.position = 'fixed';
    container.style.zIndex = '40';
    const size = { width: declared.spec.width ?? POPOVER_DEFAULT.width, height: declared.spec.height ?? POPOVER_DEFAULT.height };
    const viewport = { width: win.innerWidth, height: win.innerHeight };
    const anchorRect = anchor?.getBoundingClientRect() ?? {
      left: viewport.width / 2,
      top: viewport.height - 100,
      width: 0,
      height: 0,
    };
    const placed = placePopover(anchorRect, size, viewport);
    container.style.left = `${placed.left}px`;
    container.style.top = `${placed.top}px`;
    container.style.width = `${placed.width}px`;
    container.style.height = `${placed.height}px`;
    frame.style.width = '100%';
    frame.style.height = '100%';
    frame.style.display = 'block';
    container.appendChild(frame);
    instance.container = container;
    mounts.popoverLayer.appendChild(container);

    const dismiss: DismissibleLayerCleanup = installDismissibleLayer({
      isOpen: () => entry.surfaces.get(surfaceId) === instance,
      getInsideNodes: () => [container, anchor],
      getPopupNodes: () => [container],
      onDismiss: () => closeSurface(pluginId, surfaceId),
      getOpener: () => anchor,
      document: doc,
    });
    instance.cleanup.push(dismiss);
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') closeSurface(pluginId, surfaceId);
    };
    doc.addEventListener('keydown', onKey);
    instance.cleanup.push(() => doc.removeEventListener('keydown', onKey));
  }

  function closeSurface(pluginId: string, surfaceId: string): void {
    const entry = entries.get(pluginId);
    const instance = entry?.surfaces.get(surfaceId);
    if (!entry || !instance) return;
    entry.surfaces.delete(surfaceId);
    for (const fn of instance.cleanup.splice(0)) {
      try {
        fn();
      } catch {
        /* cleanup must not throw */
      }
    }
    if (instance.frame.contentWindow) broker.detach(instance.frame.contentWindow);
    (instance.container ?? instance.frame).remove();
    broker.emitLogic(pluginId, 'ui.surface-closed', { surfaceId, kind: instance.kind });
  }

  function activateButton(pluginId: string, buttonId: string, anchor: HTMLElement | null): void {
    const entry = entries.get(pluginId);
    if (!entry) return;
    const button = entry.plugin.manifest.contributes?.toolbarButtons?.find((b) => b.id === buttonId);
    if (!button) return;
    broker.emitLogic(pluginId, 'ui.action', { source: 'toolbar', buttonId });
    if (button.opens) {
      const [, surfaceId] = button.opens.split(':', 2);
      if (surfaceId) openSurface(pluginId, surfaceId, anchor);
    }
  }

  return {
    broker,
    load,
    unload,
    loaded: () => [...entries.values()].map((e) => e.plugin),
    isLoaded: (pluginId) => entries.has(pluginId),
    buttons,
    activateButton,
    openSurface,
    closeSurface,
    emit: (pluginId, event, payload) => broker.emit(pluginId, event, payload),
    broadcast: (event, payload) => broker.broadcast(event, payload),
    deliverData: (pluginId, message) => broker.deliverData(pluginId, message),
    dispose() {
      for (const id of [...entries.keys()]) unload(id);
      win.removeEventListener('message', onMessage);
    },
  };
}
