# Third-party notices

Petal is licensed under Apache-2.0 (see `LICENSE`). It incorporates third-party
software and assets under their own licenses, inventoried below.

No dependency in any of Petal's manifests is licensed under the GPL, AGPL,
LGPL-only, SSPL, BUSL, Commons Clause, or a non-commercial Creative Commons
license.

## Vendored source (modified)

These live under `apps/desktop/vendor/` and are pinned via `[patch.crates-io]`
in `apps/desktop/src-tauri/Cargo.toml`. Each is a **modified** copy; the local
changes are documented in that directory's `PETAL_PATCH.md`, per Apache-2.0
§4(b) and the corresponding clauses of the MIT license.

| Directory | Upstream | Version | License | Texts included |
| --- | --- | --- | --- | --- |
| `vendor/screencapturekit` | https://github.com/doom-fish/screencapturekit-rs | 8.0.0 | MIT OR Apache-2.0 | `LICENSE-MIT`, `LICENSE-APACHE` |
| `vendor/livekit` | https://github.com/livekit/rust-sdks | 0.7.49 | Apache-2.0 | `LICENSE`, `NOTICE` |
| `vendor/libwebrtc` | https://github.com/livekit/rust-sdks | 0.3.38 | Apache-2.0 | `LICENSE`, `NOTICE` |
| `vendor/webrtc-sys` | https://github.com/livekit/rust-sdks | 0.3.35 | Apache-2.0 | `LICENSE`, `NOTICE.md` |
| `vendor/tauri-nspanel` | https://github.com/ahkohd/tauri-nspanel (rev `a3122e8`) | 2.1.0 | MIT OR Apache-2.0 | `LICENSE_MIT`, `LICENSE_APACHE-2.0` |

A machine-readable inventory of every dependency (CycloneDX) is committed
under `sbom/`, with the latest license/advisory audit in
`sbom/LICENSE_AUDIT.md`.

## Prebuilt binary dependency

`webrtc-sys-build` downloads a prebuilt **libwebrtc** archive from LiveKit's
GitHub releases at build time and links it into the application. libwebrtc is
Google's WebRTC implementation, distributed under the **BSD 3-Clause License**
together with the separate **Additional IP Rights Grant** ("Patent Grant")
published in the upstream WebRTC source tree at
https://webrtc.googlesource.com/src/+/main/LICENSE and
https://webrtc.googlesource.com/src/+/main/PATENTS.

The `webrtc-sys` crate additionally ships its own `NOTICE.md` covering
contributions from Shiguredo/Wandbox (Apache-2.0), arcas-io (MIT), and Unity
(Apache-2.0). Those notices are propagated by reference here.

## Rust dependencies

Approximately 720 crates resolve in `Cargo.lock`. The overwhelming majority are
`MIT`, `Apache-2.0`, or `MIT OR Apache-2.0`. Items warranting an explicit note:

| Crate(s) | License | Note |
| --- | --- | --- |
| `tauri` and `tauri-plugin-*` | Apache-2.0 OR MIT | Application framework |
| `livekit`, `livekit-api`, `livekit-protocol`, `livekit-runtime`, `webrtc-sys` | Apache-2.0 | Real-time transport |
| `tauri-nspanel` | MIT / Apache-2.0 | Vendored (see above). The crate manifest declares **no** SPDX field, but the checkout ships both `LICENSE_MIT` and `LICENSE_APACHE-2.0`; `deny.toml` carries a `licenses.clarify` entry so `cargo deny` accepts it. |
| `cssparser`, `cssparser-macros`, `selectors`, `dtoa-short`, `option-ext` | MPL-2.0 | Transitive via Tauri/wry, statically linked. MPL-2.0 is per-file copyleft: unmodified use requires source availability (satisfied by crates.io) plus this notice. It imposes no condition on Petal's own license. |

## npm dependencies

Across the desktop, browser-client, backend, and docs-site manifests the
dependency graph is predominantly MIT, with Apache-2.0 for the LiveKit client
and server SDKs and ISC/BSD for a minority. Items warranting an explicit note:

| Package(s) | License | Note |
| --- | --- | --- |
| `livekit-client`, `livekit-server-sdk` | Apache-2.0 | Browser and backend transport |
| `@userdispatch/sdk` | **undeclared** | First-party (Kiruna Labs) package; license field to be added in 1.0.2. Used by the desktop app and browser client. Publishes no `license` field, license file, or registry license metadata. **Redistribution grant unresolved** — tracked in `sbom/LICENSE_AUDIT.md` §1; must be cleared (upstream SPDX release, written grant, or replacement) before the public release. |
| `@sentry/*` | MIT | SDKs only. (The Sentry *server* product is BUSL; the client SDKs are not.) |
| `@img/sharp-libvips-*` (via `sharp`) | LGPL-3.0-or-later | Docs-site build-time image optimization only. Dynamically loaded native module, invoked as a build tool; generated output is not a derivative work. Not linked into the desktop app or the browser client. |
| `lightningcss*` | MPL-2.0 | Build-time only |

## Fonts

Four families are redistributed as WOFF2 subsets under the **SIL Open Font
License, Version 1.1**. The full license text and copyright lines accompany the
binaries in `apps/desktop/src/assets/fonts/OFL.txt` and
`web-harness/src/assets/fonts/OFL.txt`.

| Family | Copyright |
| --- | --- |
| Manrope | Copyright 2019 The Manrope Project Authors |
| Albert Sans | Copyright 2021 The Albert Sans Project Authors |
| JetBrains Mono | Copyright 2020 The JetBrains Mono Project Authors |
| Fredoka | Copyright 2016 The Fredoka Project Authors |

None of these families declares a Reserved Font Name, so the subsetting applied
here is permitted without renaming.

## Trademarks

The Petal name and logo are not licensed under Apache-2.0. See `TRADEMARKS.md`.
