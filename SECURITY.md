# Security policy

## Reporting a vulnerability

**Please do not open a public issue for a security problem.**

Report privately through GitHub's **Report a vulnerability** button on the
repository's Security tab (GitHub Private Vulnerability Reporting). That opens
a private thread with the maintainers.

Please include what you were doing, what happened, and — if you have one — a
minimal reproduction. We will acknowledge within a few days and keep you
updated as we work on it. If you want credit in the fix, say so; we're glad to.

**Do not test against Petal's hosted infrastructure** (`app.petal.live`,
`meet.petal.live`, or the LiveKit deployment behind them). Run a local or
self-hosted stack instead — see [`docs/SELF_HOSTING.md`](docs/SELF_HOSTING.md).
Testing against hosted infrastructure affects other people's meetings and is
not authorized.

## Supported versions

Petal is pre-1.0 and ships from a single release line. Only the latest release
receives security fixes. There is no long-term-support branch.

## Scope

In scope:

- **The desktop app** (`apps/desktop/`) — screen capture, the native window
  compositor, the `petal://` URL scheme handler, permission handling.
- **Remote control** (`remote_control.rs`) — the input-injection path and its
  authorization model.
- **The token backend** (`backend/`) — token minting, scope and TTL of issued
  grants, room discovery, the admin endpoint.
- **The browser client** (`web-harness/`).
- **The updater** — feed handling and signature verification.
- **Self-hosted deployments** built from this source.

Out of scope:

- LiveKit itself — report upstream at https://github.com/livekit/rust-sdks.
- Vercel, Apple, or other third-party platform issues.
- Findings that require an attacker to already have local code execution or
  admin rights on the victim's Mac.
- Missing hardening with no demonstrated impact.

## What Petal does and does not protect

Please read this before reporting — some of it is design, not a bug.

**Media is not end-to-end encrypted.** Audio and video are encrypted in transit
(WebRTC/DTLS-SRTP), but the SFU terminates that encryption in order to route
streams. **Whoever operates the SFU can in principle access media content.** For
the hosted service that is Petal's LiveKit deployment; for a self-hosted
deployment it is you. If you need E2EE, Petal does not currently provide it.

**Remote control is a deliberate, powerful capability.** Granting control lets a
remote participant inject synthetic keyboard and mouse events into the shared
window. This is a core feature, not an oversight. It requires macOS Accessibility
permission and an explicit grant from the person sharing. Reports about the
*existence* of input injection will be closed; reports about **bypassing the
grant**, escaping the intended target window, privilege escalation, or a remote
peer obtaining control without consent are very much in scope. See
[`docs/remote-control-trust-model.md`](docs/remote-control-trust-model.md).

**Anyone with a room's access code can join it.** Access codes derive the room
credential (see [`docs/CONTRACTS.md`](docs/CONTRACTS.md)), so they are secrets —
treat an invite link like a password. There is no per-participant
authentication beyond possession of the code.

**Diagnostics.** Official builds may report crashes and diagnostics to the
maintainers' error-tracking provider. Builds from source have no such
configuration unless you supply one. See [`PRIVACY.md`](PRIVACY.md).
