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
import { PLUGIN_LIMITS, createRateLimiter } from '@petal/shared/plugin-host/rateLimit';
import { parsePluginTopic } from '@petal/shared/plugin-host/topics';
import { pluginsFromMetadata } from '@petal/shared/plugin-host/metadata';

export interface PluginsHook {
  host: PluginHost;
  installed: InstalledPlugin[];
  roomConnected(room: Room): void;
  roomDisconnected(): void;
  /** Inbound `plugin/*` packet from the connection's topic dispatcher. */
  onData(payload: Uint8Array, participant: LkParticipant | undefined, topic: string, senderIdentity: string | undefined): void;
  /** A participant's metadata changed; diff its `plugins` key into state.changed events. */
  onMetadata(participant: LkParticipant): void;
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
    room.on(RoomEvent.ParticipantConnected, onMetadata);
    room.on(RoomEvent.ParticipantDisconnected, left);
    room.on(RoomEvent.ParticipantDisconnected, forgetParticipant);
    room.on(RoomEvent.ParticipantNameChanged, (_name, p) => changed(p));
    room.on(RoomEvent.TrackMuted, (_pub, p) => changed(p));
    room.on(RoomEvent.TrackUnmuted, (_pub, p) => changed(p));
    room.on(RoomEvent.ActiveSpeakersChanged, speakers);
    room.on(RoomEvent.Reconnecting, phase);
    room.on(RoomEvent.Reconnected, phase);
    // Advertising and the late-joiner seed both need a CONNECTED room, and
    // this runs BEFORE `connect()` is awaited (connection.ts hands the Room
    // over first so these listeners exist for the very first event). A
    // publish now is refused as 'unavailable' and `remoteParticipants` is
    // still empty -- and livekit-client emits no ParticipantConnected for
    // peers present in the join response -- so do both on Connected. Redo
    // them after a full reconnect too, when metadata may need re-asserting.
    const seedAndReadvertise = () => {
      for (const p of room.remoteParticipants.values()) onMetadata(p);
      host.readvertise();
    };
    room.on(RoomEvent.Connected, seedAndReadvertise);
    room.on(RoomEvent.Reconnected, seedAndReadvertise);
    if (room.state === 'connected') seedAndReadvertise();
    // Whole-blob participant metadata is last-writer-wins across ALL local
    // writers, not just ours: connection.ts's palette-index merge right after
    // connect reads a blob that has not echoed our `plugins` key yet and
    // overwrites it (seen live: the key was gone every time after connect).
    // Reconcile whenever OUR metadata comes back without an entry for a
    // loaded meeting plugin; debounced so a burst of writes causes one redo.
    let reconcileTimer: ReturnType<typeof setTimeout> | null = null;
    const reconcileOwnAdverts = (_metadata: string | undefined, participant: LkParticipant) => {
      if (participant !== room.localParticipant || reconcileTimer) return;
      const adverts = pluginsFromMetadata(participant.metadata);
      const missing = host.loaded().some((p) => p.manifest.scope === 'meeting' && !adverts[p.manifest.id]);
      if (!missing) return;
      reconcileTimer = setTimeout(() => {
        reconcileTimer = null;
        host.readvertise();
      }, 250);
    };
    room.on(RoomEvent.ParticipantMetadataChanged, reconcileOwnAdverts);
    unsubscribe = () => {
      if (reconcileTimer) clearTimeout(reconcileTimer);
      reconcileTimer = null;
      room.off(RoomEvent.ParticipantMetadataChanged, reconcileOwnAdverts);
      room.off(RoomEvent.ParticipantConnected, joined);
      room.off(RoomEvent.ParticipantConnected, onMetadata);
      room.off(RoomEvent.ParticipantDisconnected, left);
      room.off(RoomEvent.ParticipantDisconnected, forgetParticipant);
      room.off(RoomEvent.ActiveSpeakersChanged, speakers);
      room.off(RoomEvent.Reconnecting, phase);
      room.off(RoomEvent.Reconnected, phase);
      room.off(RoomEvent.Connected, seedAndReadvertise);
      room.off(RoomEvent.Reconnected, seedAndReadvertise);
    };
    phase();
  }
  // Other participants' `plugins` adverts (metadata.ts) are owned by the
  // shared host; this file only feeds it from LiveKit participant events.
  function onMetadata(participant: LkParticipant): void {
    if (state.room && participant === state.room.localParticipant) return;
    host.applyRemoteAdverts(participant.identity, pluginsFromMetadata(participant.metadata));
  }
  function forgetParticipant(participant: LkParticipant): void {
    host.forgetParticipant(participant.identity);
  }

  function roomDisconnected(): void {
    unsubscribe?.();
    unsubscribe = null;
    host.clearRemoteAdverts();
    host.broadcast('meeting.phase', { label: dom.roomNameEl.textContent?.trim() ?? '', phase: 'disconnected' });
  }

  // Inbound plugin packets: same guards as the native bus (plugins::bus) --
  // well-formed topic, size cap, authenticated sender, per-(sender, plugin)
  // rate limit -- then the host routes to the plugin's logic frame only.
  const inbound = createRateLimiter({ perSecond: PLUGIN_LIMITS.inboundPerSenderPerSecond });
  function onData(payload: Uint8Array, participant: LkParticipant | undefined, topic: string, senderIdentity: string | undefined): void {
    const parsed = parsePluginTopic(topic);
    if (!parsed) return;
    if (payload.byteLength > PLUGIN_LIMITS.maxPayloadBytes) return;
    const room = state.room;
    const sender = participant ?? (senderIdentity && room ? room.remoteParticipants.get(senderIdentity) : undefined);
    if (!sender) return; // no authenticated sender = nothing a plugin may trust
    if (!inbound.tryTake(`${sender.identity}\u0000${parsed.pluginId}`)) return;
    if (!host.isLoaded(parsed.pluginId)) return; // fallback suggestion trigger lands in I-6
    host.deliverData(parsed.pluginId, { sub: parsed.sub, sender: participantFromLiveKit(sender, false), payload });
  }

  return { host, installed, roomConnected, roomDisconnected, onData, onMetadata };
}
