# Petal Docs (docs.petal.live)

Astro Starlight site for **using and self-hosting Petal** — install, permissions,
meetings, sharing, remote control, self-hosting, and the backend API. This is
**not** a contributor/internals site: architecture, wire-protocol contracts,
crash classes, and release engineering live in the main repo's `docs/`,
not here. The page inventory and the AUTO/OWNED content-ownership model are
described below.

## Hard rule: no PII, ever

This site must never publish personal information — Apple ID emails,
keychain-profile names, or any other individually-identifying operational
detail. This is enforced mechanically by `npm run check:pii` against the
**built** output (`dist/`), not just by picking safe source pages. See
`scripts/scan-for-pii.mjs`.

## Content ownership model

Every page under `src/content/docs/` is either:

- **OWNED** — written directly for this site, verified against the current
  desktop UI/source when it references behavior.
- **AUTO** — generated at build time from a source-of-truth doc that already
  exists elsewhere in the repo (`docs/SELF_HOSTING.md`, `backend/README.md`,
  `web-harness/README.md`), so the docs site and the in-repo docs read from
  one file, not two that can drift apart.

`scripts/sync-auto-content.mjs` does the AUTO pull + transform (strips
internal issue-number references, redacts known personal names) and writes
the generated pages/fragments. **The generated files are gitignored** — they
are always rebuilt from current source, and the thing that's actually
committed is a content-hash pin in `scripts/auto-content-manifest.json`. See
that script's header comment for the full mode list (`--check-drift`,
`--update-manifest`).

## Commands

Run from `site/`:

| Command | Action |
| --- | --- |
| `npm install` | Install dependencies |
| `npm run dev` | Sync AUTO content, then start the dev server at `localhost:4321` |
| `npm run build` | Sync AUTO content, then build the production site to `./dist/` |
| `npm run preview` | Preview the built site locally |
| `npm run sync:auto` | Regenerate AUTO-pulled pages/fragments from current source |
| `npm run check:drift` | CI gate: fail if an AUTO source changed since it was last pinned |
| `npm run check:pii` | CI gate: scan the built `dist/` output for PII/secret-shaped strings |

`scripts/ci-local.sh` at the repo root runs the full sequence (install, sync,
build, PII scan; the link validator runs automatically as part of `astro
build` via the `starlight-links-validator` plugin in `astro.config.mjs`).

## Adding a page

1. Decide OWNED vs AUTO. If AUTO, add an entry to `scripts/sync-auto-content.mjs`'s
   `ENTRIES` array and a matching manifest entry instead of hand-writing the page.
2. Add the page under `src/content/docs/<section>/<slug>.md` with Starlight
   frontmatter (`title`, `description`).
3. Add it to the sidebar in `astro.config.mjs`.
4. Run `npm run build` — the link validator will catch any dangling
   cross-references.
