# Petal patch to vendored `webrtc-sys-build` 0.3.18

Vendored from crates.io, pinned via `[patch.crates-io]` in
`apps/desktop/src-tauri/Cargo.toml` and `apps/desktop/src-tauri/harness/Cargo.toml`.

## Patch: zip 0.6 -> ^9.0.5

The only change is the `zip` dependency in `Cargo.toml`. It clears
GHSA-m665-v7pm-7grj (critical), GHSA-q7v9-x93c-xqv3 (high) and
GHSA-8hxf-qr8p-62x4 (medium). This crate's only zip use is extracting the
prebuilt libwebrtc archive at build time. Its call sites are identical to
upstream 0.3.20, which already uses zip 9.

## Why this is not just a version bump

Upstream moved to zip 9 in 0.3.20, but `WEBRTC_TAG` moves with its releases:

| Version | `WEBRTC_TAG` | zip |
|---|---|---|
| 0.3.18 | `webrtc-51ef663` | 0.6 |
| 0.3.19 | `webrtc-89d790b` | 0.6 |
| 0.3.20 | `webrtc-89d790b` | 9.0.4 |
| 0.3.21 | `webrtc-a545de4` | 9.0.4 |

Bumping would replace the prebuilt libwebrtc underneath the `webrtc-sys =0.3.35`
pin and its Petal patches. Keeping 0.3.18 keeps the same `webrtc-51ef663` binary.

## When to drop this

Remove this copy and both `[patch.crates-io]` entries when the livekit stack
moves to a `webrtc-sys` release that requires `webrtc-sys-build` >= 0.3.20.
