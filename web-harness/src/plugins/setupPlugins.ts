// Wires the shared plugin host into the browser client: mounts (hidden logic
// container, overlay over the tiles, popover layer), the toolbar cells the
// host draws for plugin buttons, and the room-event bridge that turns LiveKit
// participant changes into `meeting.*` plugin events. Design:
// plugins/README.md §2.7.

import { RoomEvent, type Participant as LkParticipant, type Room } from 'livekit-client';
import type { HarnessContext } from '../context.ts';
import { builtinPlugins } from '@petal/shared/plugin-host/builtins';
import { createPluginHost, type PluginHost } from '@petal/shared/plugin-host/host';
import type { LoadedPlugin } from '@petal/shared/plugin-host/broker';
import { pluginIconSvg } from '@petal/shared/plugin-host/icons';
import { hostCompatibility } from '@petal/shared/plugin-host/manifest';
import { isPluginEnabled, readEnabledOverrides, type InstalledPlugin } from '@petal/shared/plugin-host/settingsModel';
import { badgeText, type ToolbarButtonModel } from '@petal/shared/plugin-host/surfaces';
import { createWebAdapter, participantFromLiveKit } from './webAdapter.ts';

export interface PluginsHook {
  host: PluginHost;
  installed: InstalledPlugin[];
  roomConnected(room: Room): void;
  roomDisconnected(): void;
}

declare const __PETAL_BUILD_INFO__: { version: string } | undefined;

function hostVersion(): string {
  try {
    return typeof __PETAL_BUILD_INFO__ !== 'undefined' && __PETAL_BUILD_INFO__?.version ? __PETAL_BUILD_INFO__.version : '0.0.0';
  } catch {
    return '0.0.0';
  }
}

export function setupPlugins(ctx: HarnessContext): PluginsHook {
  const { dom, ui, state } = ctx;
  const doc = document;

  const logic = doc.createElement('div');
  logic.className = 'plugin-logic-frames';
  logic.hidden = true;
  doc.body.appendChild(logic);

  const overlay = doc.createElement('div');
  overlay.className = 'plugin-overlay';
  dom.meetingScreen.appendChild(overlay);

  const popoverLayer = doc.createElement('div');
  popoverLayer.className = 'plugin-popover-layer';
  doc.body.appendChild(popoverLayer);

  const controlsLeft = dom.ctlDraw.closest('.controls-left') as HTMLElement | null;
  const cells = new Map<string, HTMLElement>();

  const adapter = createWebAdapter({
    room: () => state.room,
    roomLabel: () => dom.roomNameEl.textContent?.trim() ?? '',
    toast: (text) => ui.showToast(text),
    log: (line, kind) => ui.logEvent(line, kind),
  });

  function renderButtons(buttons: ToolbarButtonModel[]): void {
    if (!controlsLeft) return;
    const wanted = new Set<string>();
    for (const button of buttons) {
      const key = `${button.pluginId}/${button.buttonId}`;
      wanted.add(key);
      let cell = cells.get(key);
      if (!cell) {
        cell = doc.createElement('div');
        cell.className = 'control-cell plugin-control-cell';
        cell.dataset.plugin = button.pluginId;
        cell.dataset.button = button.buttonId;
        const btn = doc.createElement('button');
        btn.type = 'button';
        btn.className = 'control-button plugin-control-button';
        btn.addEventListener('click', () => host.activateButton(button.pluginId, button.buttonId, btn));
        const icon = doc.createElement('span');
        icon.className = 'plugin-control-icon';
        const badge = doc.createElement('span');
        badge.className = 'plugin-control-badge';
        badge.hidden = true;
        btn.append(icon, badge);
        const label = doc.createElement('span');
        label.className = 'meeting-control-label';
        cell.append(btn, label);
        controlsLeft.appendChild(cell);
        cells.set(key, cell);
      }
      const btn = cell.querySelector('button')!;
      btn.setAttribute('aria-label', button.ariaLabel);
      btn.disabled = button.disabled;
      if (button.opens) btn.setAttribute('aria-haspopup', 'dialog');
      cell.querySelector('.plugin-control-icon')!.innerHTML = pluginIconSvg(button.icon, 20);
      const badge = cell.querySelector<HTMLElement>('.plugin-control-badge')!;
      const text = badgeText(button.badge);
      badge.hidden = text === null;
      badge.textContent = text ?? '';
      cell.querySelector('.meeting-control-label')!.textContent = button.label;
    }
    for (const [key, cell] of cells) {
      if (!wanted.has(key)) {
        cell.remove();
        cells.delete(key);
      }
    }
  }

  const host = createPluginHost({
    document: doc,
    adapter,
    hostVersion: hostVersion(),
    mounts: { logic, overlay, popoverLayer },
    onButtonsChanged: renderButtons,
    warn: (message) => ui.logEvent(message, 'warn'),
  });

  const installed = builtinPlugins((message) => ui.logEvent(message, 'error'));
  const overrides = readEnabledOverrides(typeof localStorage === 'undefined' ? undefined : localStorage);
  for (const plugin of installed) {
    if (!isPluginEnabled(plugin, overrides)) continue;
    const compat = hostCompatibility(plugin.manifest, hostVersion());
    if (!compat.ok) {
      // Dev builds report version "dev"/"test"; run built-ins anyway there.
      if (!/^\d/.test(hostVersion()) === false) {
        ui.logEvent(`plugin ${plugin.manifest.id} skipped: ${compat.reason}`, 'warn');
        continue;
      }
    }
    const loaded: LoadedPlugin = { manifest: plugin.manifest, granted: plugin.manifest.permissions, source: plugin.source };
    host.load(loaded, plugin.source_js);
  }

  // Room bridge: LiveKit participant events -> meeting.* plugin events.
  let unsubscribe: (() => void) | null = null;
  function roomConnected(room: Room): void {
    roomDisconnected();
    const joined = (p: LkParticipant) => host.broadcast('meeting.participant-joined', participantFromLiveKit(p, false));
    const left = (p: LkParticipant) => host.broadcast('meeting.participant-left', participantFromLiveKit(p, false));
    const changed = (p: LkParticipant) => host.broadcast('meeting.participant-changed', participantFromLiveKit(p, p === room.localParticipant));
    const speakers = () => {
      for (const p of [room.localParticipant, ...room.remoteParticipants.values()]) changed(p);
    };
    const phase = () => host.broadcast('meeting.phase', adapter.meeting!.room());
    room.on(RoomEvent.ParticipantConnected, joined);
    room.on(RoomEvent.ParticipantDisconnected, left);
    room.on(RoomEvent.ParticipantNameChanged, (_name, p) => changed(p));
    room.on(RoomEvent.TrackMuted, (_pub, p) => changed(p));
    room.on(RoomEvent.TrackUnmuted, (_pub, p) => changed(p));
    room.on(RoomEvent.ActiveSpeakersChanged, speakers);
    room.on(RoomEvent.Reconnecting, phase);
    room.on(RoomEvent.Reconnected, phase);
    unsubscribe = () => {
      room.off(RoomEvent.ParticipantConnected, joined);
      room.off(RoomEvent.ParticipantDisconnected, left);
      room.off(RoomEvent.ActiveSpeakersChanged, speakers);
      room.off(RoomEvent.Reconnecting, phase);
      room.off(RoomEvent.Reconnected, phase);
    };
    phase();
  }
  function roomDisconnected(): void {
    unsubscribe?.();
    unsubscribe = null;
    host.broadcast('meeting.phase', { label: dom.roomNameEl.textContent?.trim() ?? '', phase: 'disconnected' });
  }

  return { host, installed, roomConnected, roomDisconnected };
}
