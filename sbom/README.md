# Software bill of materials

CycloneDX 1.5 JSON, one file per dependency root, committed and kept in sync
with the lockfiles by CI (`.github/workflows/sbom.yml` regenerates and fails
on any diff, so a lockfile change must land together with its SBOM).

| File | Root | Generator | Scope |
| --- | --- | --- | --- |
| `desktop-rust.cdx.json` | `apps/desktop/src-tauri` (crate `desktop`) | `cargo cyclonedx --all --target all` | Full transitive graph for every shipped target (macOS arm64/x86_64 + Windows), from `Cargo.lock` |
| `desktop-npm.cdx.json` | `apps/desktop` | `npm sbom` | Full graph from `package-lock.json` (dev + prod) |
| `backend-npm.cdx.json` | `backend` | `npm sbom` | Full graph from `package-lock.json` |
| `web-harness-npm.cdx.json` | `web-harness` | `npm sbom` | Full graph from `package-lock.json` |
| `site-npm.cdx.json` | `site` | `npm sbom` | Full graph from `package-lock.json` (docs site, build-time only) |

Regenerate with `scripts/generate-sbom.sh` (needs `cargo install
cargo-cyclonedx` and `npm ci --ignore-scripts` in each npm root). Output is
normalised — timestamps, serial numbers and tool versions stripped, keys
sorted — so two runs on the same lockfiles are byte-identical.

## Not covered by a generated SBOM

- **`shared/`** — first-party Svelte/TS consumed by path from `apps/desktop`
  and `web-harness`; it has no manifest and declares no dependencies of its
  own. Covered by `LICENSE`.
- **Prebuilt libwebrtc** — `webrtc-sys-build` downloads a prebuilt libwebrtc
  archive from LiveKit's GitHub releases at build time and links it in. It is
  not a Cargo package and does not appear in `desktop-rust.cdx.json`. BSD
  3-Clause + the WebRTC patent grant; see `THIRD_PARTY_NOTICES.md`.
- **Bundled fonts** — Manrope, Albert Sans, JetBrains Mono, Fredoka (OFL-1.1)
  under `apps/desktop/src/assets/fonts/` and `web-harness/src/assets/fonts/`,
  with the license text in each directory's `OFL.txt`.

## Vendored (modified) crates

These appear in `desktop-rust.cdx.json` as path dependencies
(`purl` without a registry), pinned through `[patch.crates-io]` in
`apps/desktop/src-tauri/Cargo.toml`. Each directory carries the upstream
license and a `PETAL_PATCH.md` describing exactly what was changed.

| Directory | Upstream | Version | License | Patch notes |
| --- | --- | --- | --- | --- |
| `apps/desktop/vendor/screencapturekit` | https://github.com/doom-fish/screencapturekit-rs | 8.0.0 | MIT OR Apache-2.0 | `apps/desktop/vendor/screencapturekit/PETAL_PATCH.md` |
| `apps/desktop/vendor/livekit` | https://github.com/livekit/rust-sdks | 0.7.49 | Apache-2.0 | `apps/desktop/vendor/livekit/PETAL_PATCH.md` |
| `apps/desktop/vendor/libwebrtc` | https://github.com/livekit/rust-sdks | 0.3.38 | Apache-2.0 | `apps/desktop/vendor/libwebrtc/PETAL_PATCH.md` |
| `apps/desktop/vendor/webrtc-sys` | https://github.com/livekit/rust-sdks | 0.3.35 | Apache-2.0 | `apps/desktop/vendor/webrtc-sys/PETAL_PATCH.md` |
| `apps/desktop/vendor/tauri-nspanel` | https://github.com/ahkohd/tauri-nspanel (rev `a3122e8`) | 2.1.0 | MIT OR Apache-2.0 (no SPDX field in manifest; both license files shipped) | `apps/desktop/vendor/tauri-nspanel/PETAL_PATCH.md` |

## License and advisory audit

`LICENSE_AUDIT.md` in this directory records the most recent `cargo deny`
and `license-checker` results. Run them yourself with:

```bash
cargo deny --manifest-path apps/desktop/src-tauri/Cargo.toml check licenses
cargo deny --manifest-path apps/desktop/src-tauri/Cargo.toml check advisories
(cd apps/desktop && npx license-checker-rseidelsohn --summary --production)
```

The allowlist lives in `deny.toml` at the repo root.
