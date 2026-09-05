# License and advisory audit — 2026-08-22

Run on `main` @ `bbefc0eb` (lockfiles unchanged since). Tools: `cargo-deny`
0.20.2, `license-checker-rseidelsohn` (latest via npx), against the allowlist
in `deny.toml`.

## Summary

| Root | Result | Notes |
| --- | --- | --- |
| `apps/desktop/src-tauri` (733 crates, all targets) | **licenses ok** | 1 crate with no SPDX field (`tauri-nspanel`, clarified in `deny.toml`) |
| `apps/desktop` (npm, prod) | ok, **1 unknown** | `@userdispatch/sdk` — see below |
| `backend` (npm, prod) | ok | |
| `web-harness` (npm, prod) | ok, **1 unknown** | `@userdispatch/sdk` — same |
| `site` (npm, prod) | ok | LGPL-3.0 / MPL-2.0 / BlueOak are build-time only — see below |

No GPL, AGPL, SSPL, BUSL, Commons-Clause or non-commercial license was found
in any root. `UNLICENSED` rows in the raw `license-checker` output are the
`"private": true` root packages themselves (`petal-backend`, `web-harness`,
`petal-docs-site`); each declares `"license": "Apache-2.0"`.

## Findings requiring action

### 1. `@userdispatch/sdk@1.0.1` — no license declared (desktop + web-harness)

> **Resolved as non-blocking, 2026-08-22.** `@userdispatch/sdk` is **Kiruna Labs' own package** (npm maintainer `kiruna-labs`, userdispatch.com is a Kiruna product), so the missing `license` field is housekeeping, not a third-party risk — user decision 2026-08-22: NOT a publish blocker. Follow-up: publish 1.0.2 with an SPDX `license` + `repository` field and bump both lockfiles.

Used by `apps/desktop` (`^1.0.1`) and `web-harness` (`1.0.1`). The package
has **no `license` field** in `package.json`, no `LICENSE` file, no license
statement in its README, and the npm registry metadata carries none either.
Under copyright default that means *no grant to redistribute*, which is a
real problem for an Apache-2.0 release that bundles it into the shipped app
and the browser client.

Recommended action (pick one, in order of preference):
1. Ask the UserDispatch maintainers to publish a release with an SPDX
   `license` field (MIT/Apache-2.0), and pin to it.
2. Obtain a written license grant from UserDispatch for redistribution in
   Petal and record it in `THIRD_PARTY_NOTICES.md`.
3. Replace the dependency with a first-party client for whatever endpoints
   it wraps (it is a thin SDK; `VITE_USERDISPATCH_PUBLIC_KEY` is the only
   config it needs).

Until resolved, `THIRD_PARTY_NOTICES.md` should state the status rather than
imply it is cleared.

### 2. Rust advisories (`cargo deny check advisories`) — 4 vulnerabilities, 5 unmaintained

| Advisory | Crate | Locked | Fix | Exposure |
| --- | --- | --- | --- | --- |
| RUSTSEC-2026-0258 | `h2` | 0.4.15 | `cargo update -p h2` → ≥0.4.16 | HTTP/2 client path (token/updater fetches). Unbounded empty DATA frames — DoS against a client is low impact, but the update is a lockfile bump. |
| RUSTSEC-2026-0194 | `quick-xml` | 0.39.4 | ≥0.41.0 (semver-major; needs the depending crate to move) | Quadratic duplicate-attribute check. Which crate pulls it in is in the cargo-deny tree; untrusted XML is not parsed by Petal directly. |
| RUSTSEC-2026-0195 | `quick-xml` | 0.39.4 | ≥0.41.0 | Namespace-allocation DoS in `NsReader`; same exposure. |
| RUSTSEC-2026-0204 | `crossbeam-epoch` | 0.9.18 | `cargo update -p crossbeam-epoch` → ≥0.9.20 | `fmt::Pointer` on an invalid pointer; not reachable from Petal code. Lockfile bump. |
| RUSTSEC-2025-0075/0080/0081/0098/0100 | `unic-*` 0.9.0 | — | No upgrade; depending crate must migrate | Unmaintained, not a vulnerability. |

Recommended action: land the two plain `cargo update -p` bumps (h2,
crossbeam-epoch) in one `chore(deps)` commit with `ci-local.sh` green; file
an issue for the `quick-xml` / `unic-*` parents (requires the upstream
dependents to move major versions — identify them with
`cargo tree -i quick-xml` and `cargo tree -i unic-ucd-ident`).

## Noted, no action needed

| Package | License | Why it is fine |
| --- | --- | --- |
| `tauri-nspanel` (vendored) | MIT OR Apache-2.0 | Manifest lacks an SPDX field; `LICENSE_MIT` + `LICENSE_APACHE-2.0` ship in the checkout. Clarified in `deny.toml`; recorded in `THIRD_PARTY_NOTICES.md`. |
| `cssparser`, `selectors`, `dtoa-short`, `option-ext` (Rust) | MPL-2.0 | File-level copyleft, used unmodified from crates.io. No condition on Petal's license. |
| `lightningcss*` (site) | MPL-2.0 | Build tool for the docs site only. |
| `@img/sharp-libvips-darwin-arm64` (site) | LGPL-3.0-or-later | Image optimisation at docs-site build time; dynamically loaded native module, not linked into any shipped Petal artifact. |
| `common-ancestor-path`, `lru-cache`, `sax` (site) | BlueOak-1.0.0 | Permissive (OSI-approved). Build-time only. |
| `argparse` (site) | Python-2.0 | Permissive. Build-time only. |
| `@bufbuild/protobuf` | Apache-2.0 AND BSD-3-Clause | Both permissive. |

## How to re-run

```bash
scripts/generate-sbom.sh                                  # refresh sbom/*.cdx.json
cargo deny --manifest-path apps/desktop/src-tauri/Cargo.toml check licenses advisories
for r in apps/desktop backend web-harness site; do
  (cd "$r" && npx license-checker-rseidelsohn --summary --production)
done
```

Update this file's date and tables when the result changes.
