# Writing a Petal plugin

Status: M1 (built-in plugins, sandboxed frames, toolbar buttons, popover and
overlay surfaces) is in `main`. The design and milestone status live in
[`plugins/README.md`](../plugins/README.md).
This guide grows as milestones land; sections marked *(M1)* describe what
exists once M1 merges.

## What a plugin is *(M1)*

A plugin is a directory with a `manifest.json` and one ES module entry file.
It runs in a sandboxed frame inside Petal, on both the desktop app and the
browser client, and talks to Petal only through the `petal` object handed to
`activate`. It has no network access, no filesystem, and no Tauri access; each
capability it uses must be declared as a permission in the manifest and is
brokered by the host.

```ts
import { definePlugin } from '@petal/plugin-sdk';

export default definePlugin({
  activate(petal) {
    petal.log.info('hello from', petal.plugin.id);
  },
});
```

## Layout *(M1)*

```
my-plugin/
  manifest.json
  src/index.ts
  vite.config.ts      # copy from plugins/sdk/vite.lib.ts
  dist/plugin.js      # built output, single ESM
```

Build with `vite build` (use `pluginLibConfig` from `@petal/plugin-sdk/vite`
so the output is one self-contained ES module). Pack with
`node plugins/build-all.mjs <dir>` to get a `bundle.json`, which is what the
registry publishes and what Petal installs.

Petal's own built-in plugins (`plugins/reactions/` and friends) skip the build
step entirely: each is one plain `plugin.js` with no imports that registers
via the `globalThis.__petalRegister` hook. That is only because the clients
compile them in directly; write yours with the SDK.

## What runs where *(M1)*

Every enabled plugin gets one hidden sandboxed frame running your `activate`.
Each UI surface you declare (`popover`, `overlay`; `panel` and `settings` come
later) is a separate frame of the same module where `mountSurface(petal,
surface)` runs; talk to your logic frame over `surface.channel` /
`petal.ui.channel(surfaceId)`. Toolbar buttons are drawn by Petal from your
manifest; clicks arrive via `petal.ui.onAction` and a button with `opens`
toggles that surface for you.

## Talking to the rest of the meeting *(M2)*

A `meeting`-scoped plugin with `data:publish` sends and receives messages on
its own namespace:

```ts
petal.data.publish('emoji', { e: '👍' }, { reliable: false });   // topic plugin/<your id>/emoji
petal.data.on('emoji', (msg) => console.log(msg.sender.name, msg.json()));
```

Petal derives the topic from your manifest id, stamps `msg.sender` from the
authenticated LiveKit participant (never from the payload), caps payloads at
16 KB, and rate-limits both directions. Objects are JSON-encoded for you;
pass a `Uint8Array` for raw bytes. `to: [identity]` targets specific peers.

With `state:write`, `petal.state.set(value)` publishes a small (≤ 2 KB) value
in your participant metadata that everyone with your plugin can read via
`petal.state.get(identity)` and `petal.state.on(...)`. It survives late joins
(it is state, not a stream), so use it for "what am I currently doing", not
for events. Petal also uses this metadata to tell peers you are running the
plugin, which is what powers the install prompt in M3.

## How users see your plugin

Every control Petal draws for you carries a small puzzle badge whose tooltip
names your plugin AND where Petal loaded it from ("<name> · built-in plugin",
"· installed plugin", "· dev plugin"); your popovers get the same line as a
caption. The source is Petal's own record, not something a manifest can
claim, so a sideloaded plugin can never present itself as a built-in one.
Right-clicking either offers "Turn off <name>", which unloads your plugin
immediately; on desktop users turn it back on in Settings → Plugins, and in
the browser the plugin comes back on the next page load. Design your plugin
so that being switched off mid-meeting is harmless: keep anything worth
keeping in `petal.storage`, and expect `activate` to run again later.

Your `name` and every button `label` must be printable text: line breaks,
control characters, and bidi overrides/isolates are refused by
`validateManifest`, because they can rearrange the words Petal draws around
them. Petal also clamps a declared popover `width`/`height` to a floor, so
its caption always has room.

## Manifest reference *(M1)*

See `shared/plugin-host/manifest.ts` for the authoritative TypeScript type
(re-exported by `@petal/plugin-sdk`) and
`plugins/README.md` §2.2 for the annotated example and validation rules.

## Loading your plugin while developing *(M4)*

Settings → Plugins → Developer mode, then point Petal at your plugin folder
(desktop) or a local dev-server URL (desktop and web). Changes reload
automatically.

## Publishing *(M3)*

Submit the `bundle.json`. Publishing to the official registry is handled by
the core team and requires the plugin to pass review; the app only installs
bundles whose signature verifies against the registry key it was built with.
