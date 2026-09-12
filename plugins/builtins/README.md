# Vendored built-in plugin bundles

Each `<id>/bundle.json` here is the deterministic artifact produced by
`build-all.mjs` in `kiruna-labs/petal-plugins` at the commit recorded in
`SOURCES.json`. The clients compile these in as built-ins
(`shared/plugin-host/builtins.ts`); the registry serves the same bytes, so a
built-in is a preinstalled registry plugin (`plugins/README.md` §2.11, §2.13).

Rules:
- Never edit a bundle by hand. To change a built-in, change its source in the
  plugins repo, then replace the bundle here with that commit's CI artifact
  and update `SOURCES.json` in the same PR. `builtins.test.mjs` fails if the
  bundle is not in canonical form or its manifest disagrees with `SOURCES.json`.
- Signatures (`bundle.json.minisig`, verified against the registry public key
  in a test) arrive once the production registry key exists; until then
  provenance is the recorded commit.
- This directory must not be named `dist`: `scripts/deploy-web-harness.sh`
  excludes `dist` when staging the web client.
