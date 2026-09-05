# Writing a Petal plugin

Status: the plugin system is under construction on `feature/plugin-system`.
The design and current status live in [`plugins/README.md`](../plugins/README.md).
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

## Manifest reference *(M1)*

See `plugins/sdk/src/manifest.ts` for the authoritative TypeScript type and
`plugins/README.md` §2.2 for the annotated example and validation rules.

## Loading your plugin while developing *(M4)*

Settings → Plugins → Developer mode, then point Petal at your plugin folder
(desktop) or a local dev-server URL (desktop and web). Changes reload
automatically.

## Publishing *(M3)*

Submit the `bundle.json`. Publishing to the official registry is handled by
the core team and requires the plugin to pass review; the app only installs
bundles whose signature verifies against the registry key it was built with.
