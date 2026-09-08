# Plugin registry contract fixtures

The registry is a **static, minisign-signed tree** (plugins/README.md §2.9):

```
<REGISTRY_URL>/index.json            + index.json.minisig
<REGISTRY_URL>/plugins/<id>/<version>/bundle.json   + bundle.json.minisig
```

These files are a complete sample signed with a **throwaway test key** whose
public half is `test.pub`. Nothing outside tests trusts that key; a real build
trusts only the key baked in at build time (`PETAL_PLUGIN_REGISTRY_PUBKEY` /
`VITE_PETAL_PLUGIN_REGISTRY_PUBKEY`). The secret half was discarded after
signing; regenerate everything with `node contracts/plugin-registry/gen-fixtures.mjs`.

Pinned on both clients:

- `web-harness/tests/pluginRegistry.test.ts` — `shared/plugin-host/{minisign,registry}.ts`
- `apps/desktop/src-tauri/src/plugins/registry.rs` tests — `minisign-verify` + serde

The private marketplace repository vendors this directory byte-for-byte and
its publisher validates its output against it. A change here is a deliberate
two-repo event.

`index.json` shape: `{ schemaVersion: 1, generatedAt, plugins: [{ id, name,
description, publisher, latest, versions: [{ version, minHostVersion,
apiVersion, permissions, bundleUrl, sigUrl, sha256, size, verified, scan }] }] }`.
Entries with `verified: false` are listed but not installable from the UI.
