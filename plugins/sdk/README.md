# @petal/plugin-sdk

Types and the `definePlugin` helper for Petal plugins. The API surface is
documented in `docs/PLUGINS.md`; the authoritative types live in
`shared/plugin-host/api.ts` and `shared/plugin-host/manifest.ts` and are
re-exported here so a plugin never imports from the monorepo directly.

```ts
import { definePlugin } from '@petal/plugin-sdk';

export default definePlugin({
  activate(petal) {
    petal.log.info('hello');
  },
});
```

Build with the shared Vite config (`plugins/sdk/vite.lib.ts`) to a single
`dist/plugin.js`; `build-all.mjs` in `kiruna-labs/petal-plugins` packs it into
`bundle.json` (plugin source lives in that repo, not here).
