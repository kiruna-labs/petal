# Contributing to Petal

Thanks for looking. Petal is a young, opinionated project; this file covers what
you need to build it, and the handful of conventions that are non-obvious.

## Before you build: the toolchain requirement

Petal *runs* on macOS 13+, but **building it currently requires the macOS 26 SDK
(Xcode 26.x)**. The `apple-metal` crate, pulled in transitively by our vendored
`screencapturekit`, uses Metal APIs that do not exist in earlier SDKs; on an
older one the Swift bridge fails to compile with an unhelpful "cannot find
`MTLSamplerReductionMode` in scope".

You need:

- macOS with **full Xcode 26.x** (not just Command Line Tools)
- **Node 20+**
- **Rust** stable
- `livekit-server` (`brew install livekit`) for local meetings

## Running it

```bash
livekit-server --dev              # ws://localhost:7880 (devkey/secret)

cd apps/desktop
npm install
PETAL_BACKEND_URL= \
LIVEKIT_URL=ws://localhost:7880 LIVEKIT_API_KEY=devkey LIVEKIT_API_SECRET=secret \
  npm run dev:clean
```

The **empty `PETAL_BACKEND_URL=`** selects the debug build's local token mint,
so your local LiveKit credentials are actually used. There is no hosted
default — a build that never sets it has no token backend at all, and joining
fails with a message saying so. That is deliberate: a build from this source
should never quietly use someone else's service.

Use `npm run dev:clean` rather than `tauri build` for GUI iteration — it keeps
the macOS Screen Recording grant stable across rebuilds, which a full bundle
re-sign does not. Logs are at `~/Library/Logs/Petal/petal.log`.

Never test against Petal's hosted infrastructure. Run a local stack, or see
[`docs/SELF_HOSTING.md`](docs/SELF_HOSTING.md).

## The gate

```bash
scripts/ci-local.sh
```

This is the primary gate and mirrors CI: frontend check + build, backend
typecheck + tests, `cargo build` + `cargo test --lib`, browser-client build +
tests, the docs-site build, and the PII/secrets scan over the whole tree.

Two things it does that you should know about up front:

- On first run it sets `core.hooksPath` to `scripts/git-hooks`, installing a
  pre-push hook in your clone.
- It re-executes itself through a source-provenance wrapper.

Rust tests need a Swift runtime path at launch:

```bash
DYLD_FALLBACK_LIBRARY_PATH=/Library/Developer/CommandLineTools/usr/lib/swift-5.5/macosx \
  cargo test --lib
```

**Always wrap `cargo build`/`cargo test` in `timeout`, log to a file, and check
the real exit code.** A deadlock hangs forever at ~0% CPU and looks like a slow
build; piping through `tail` reports `tail`'s exit status, not the test run's.
Confirm the literal `test result:` line before believing a pass.

## Conventions that will surprise you

**Read [`docs/ENGINEERING.md`](docs/ENGINEERING.md) before touching native
window code.** It documents crash classes — AppKit off the main thread, never
`close()`-ing a `tauri_nspanel`, LiveKit calls needing an ambient tokio runtime,
`CGEventPostToPid` silently not working for pointer events — that are easy to
reintroduce and painful to debug.

**Native window lifecycle changes need a test that drives the real event path.**
A unit test on an extracted pure helper proves the arithmetic, not that anything
calls it correctly from the real event chain. This class of bug has shipped
green more than once. For geometry problems, start with
`PETAL_TRACE_PANEL_GEOMETRY=1` before theorising.

**Contracts change in lockstep.** Wire formats are shared across the Rust core,
the backend, and the browser client, and pinned by
`contracts/petal-contracts.json`. Change all sides and the fixture in the same
commit — see [`docs/CONTRACTS.md`](docs/CONTRACTS.md).

**One shared UI + logic codebase.** Shared design tokens
(`shared/ui/tokens.css`), presentational components (`shared/ui/components/`),
and pure logic (`shared/logic/` — meeting codes, join input, local echo) are
the SINGLE SOURCE, imported by BOTH the desktop app (`apps/desktop`) and the
browser client (`web-harness`) via the `@petal/shared` alias. Edit there, never
in per-client copies. `web-harness/` is a real user-facing client, not a test
rig, despite the name; its app shell (meeting tiles, control bar, remote-window
headers) is still its own, but everything shared comes from `shared/`.

**User-facing text must never truncate.** No clipped labels, no accidental
ellipsis, at the real window width. If a string doesn't fit, shrink the font,
tighten the layout, or wrap — but never ship it cut off.

**Vendored dependencies** live in `apps/desktop/vendor/` and are pinned via
`[patch.crates-io]`. If you change one, update that directory's
`PETAL_PATCH.md` with what you changed and the condition under which the patch
can be dropped.

**Don't add observability infrastructure as a deliverable.** Instrumentation is
a debugging tool: delete it in the same PR that fixes the bug it was added to
find. And prove a bug is real before fixing it — the absence of a log line is
not evidence.

## Pull requests

- Branch off `main`; it is the integration trunk.
- Tests change in the same commit as the behavior they cover.
- `scripts/ci-local.sh` green before you push.
- Explain what you verified, not just what you changed.

## Reporting bugs

Open an issue with your macOS version, chip (Apple Silicon or Intel), Petal
version, whether it was native-to-native or native-to-browser, and the relevant
excerpt from `~/Library/Logs/Petal/petal.log`.

**Security issues do not go in the tracker** — see [`SECURITY.md`](SECURITY.md).

## Licensing of contributions

Petal is Apache-2.0. By submitting a contribution you agree it is licensed under
those terms, per Apache-2.0 §5. Don't paste in code you don't have the right to
relicense.

## Commit identity

Set your identity for this repository explicitly rather than relying on your
global git config:

```
git config user.name "Your Name"
git config user.email you@example.com
```

`scripts/ci-local.sh` installs `scripts/git-hooks/` via `core.hooksPath`. Its
`pre-commit` hook refuses a commit whose `user.email` fell through to the
global config, so a machine shared with other projects or employers never
authors a Petal commit under the wrong identity by accident. Use `--no-verify`
only when you have checked the identity yourself.
