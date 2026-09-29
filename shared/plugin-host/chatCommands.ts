// Which plugin owns each slash command. Pure, so both clients and the tests
// agree: commands come from the manifests of LOADED plugins that were granted
// `chat:commands`, and a name two plugins declare goes to one owner by a fixed
// rule (built-in, then installed, then dev; then plugin id), never to
// whichever loaded last. A sideloaded plugin can therefore never shadow a
// built-in's command. Design: plugins/README.md §2.4 `chat`.

import type { ChatCommandOption } from '../logic/chat.ts';
import type { LoadedPlugin } from './broker.ts';

const SOURCE_RANK: Record<LoadedPlugin['source'], number> = { builtin: 0, registry: 1, dev: 2 };

export interface ChatCommandConflict {
  name: string;
  /** The plugin that owns `/name`. */
  winner: string;
  /** Plugins that also declared it; their `/name` is not reachable. */
  losers: string[];
}

export interface ResolvedChatCommands {
  commands: ChatCommandOption[];
  conflicts: ChatCommandConflict[];
}

export function resolveChatCommands(plugins: readonly LoadedPlugin[]): ResolvedChatCommands {
  const ranked = [...plugins]
    .filter((p) => p.granted.includes('chat:commands') && (p.manifest.contributes?.chatCommands?.length ?? 0) > 0)
    .sort((a, b) => SOURCE_RANK[a.source] - SOURCE_RANK[b.source] || a.manifest.id.localeCompare(b.manifest.id));
  const byName = new Map<string, ChatCommandOption>();
  const losers = new Map<string, string[]>();
  for (const plugin of ranked) {
    for (const c of plugin.manifest.contributes!.chatCommands!) {
      if (byName.has(c.name)) {
        losers.set(c.name, [...(losers.get(c.name) ?? []), plugin.manifest.id]);
        continue;
      }
      byName.set(c.name, {
        name: c.name,
        usage: c.usage ?? '',
        description: c.description,
        pluginId: plugin.manifest.id,
        pluginName: plugin.manifest.name,
        source: plugin.source,
      });
    }
  }
  const commands = [...byName.values()].sort((a, b) => a.name.localeCompare(b.name));
  const conflicts = [...losers].map(([name, ids]) => ({ name, winner: byName.get(name)!.pluginId, losers: ids }));
  return { commands, conflicts };
}
