# Testing — start here

New to Petal? This is the orientation guide: **what to run, what it proves, and
how to read the result without fooling yourself.** It is deliberately short.

Its two companions are references, not guides — reach for them once you know
what you are looking for:

| Doc | Answers |
|---|---|
| **this file** | "I'm new. What do I run, and what does it mean?" |
| `docs/VALIDATION.md` | "Is this evidence enough to close the issue?" |
| `docs/TESTING.md` | "How exactly do I run X, and what went wrong last time?" (2000+ lines of hard-won specifics) |

---

## The one command

```bash
./scripts/ci-local.sh
```

That is the gate. **Green here is meant to equal green in CI**, and it is what
you run before pushing. It takes roughly 4–8 minutes on a warm cache.

You do not normally run anything else. The live suites below exist for
specific questions and cost far more.

---

## What the gate actually checks

Roughly in order, each step failing the whole run:

| Step | What it proves |
|---|---|
| Version lockstep | All 9 version fields agree (incl. `Cargo.lock`'s own `desktop` entry) |
| Release script unit tests | `bump-version.mjs` / `publish-blob.mjs` pure logic |
| Source provenance | The build came from the tree it claims |
| Harness contracts | Process cleanup, capture preflight, and the live-suite instance guard — the test *harness itself* is tested |
| Frontend | `svelte-check` + ~100 test files + a real static build (`apps/desktop`) |
| Backend | `tsc --noEmit` + the five offline suites |
| Rust | `cargo build` + `cargo test --lib` (~2000 `#[test]`s), default-feature **and** cockpit-privileged; `cargo build --examples`; the vendored `tauri-nspanel` tests |
| Browser client | `web-harness` build + ~75 test files + the isolated-deploy build simulation |
| Web no-black-frame | Pixel gate across a forced gap (`scripts/verify-no-black-frame.mjs`) |
| Remote-control loopback | Check-only (no live app) |
| Docs site | `site/` builds and its link validator passes |
| PII/secrets scan | Nothing sensitive in the publishable tree |

(`scripts/ci-local.sh` is the authority — around twenty `step`s at the time
of writing; this table groups them.)

There is also a **pre-push hook** (`scripts/git-hooks/pre-push`, installed
automatically by `ci-local.sh`). Any push touching `apps/desktop/src-tauri/`,
`contracts/`, `backend/`, or the CI scripts re-runs the Rust gate
independently. It exists because a PR once merged on an incomplete local cargo
run described as "no assertion failure observed". Bypass with `--no-verify`
only for a genuine, stated reason.

---

## The tiers, by cost

Reach for the cheapest instrument that can distinguish your hypotheses.
**Never run the full live matrix to learn one fact.**

| Cost | Command | Answers |
|---|---|---|
| seconds | `grep ~/Library/Logs/Petal/petal.log` | What the app decided, and when |
| seconds | `cargo test --lib <name>` | Is this logic right in isolation |
| minutes | `./scripts/ci-local.sh` | Does everything still build and pass |
| ~90s | a passive CDP probe (see `docs/TESTING.md`) | What is actually **on the wire** |
| ~10 min + GUI | `./scripts/rc-live-suite.sh` | Does the whole path work end to end |

The last row launches Petal, Chrome and TextEdit, and runs `dev:clean` — which
kills any other Petal instance on the machine. Several agents and developers
share this repo, so the suite now **refuses to start** if a foreign `desktop`
process is running. Do not disable that guard.

---

## Reading a result honestly

This repo has lost multiple days to signals that could not distinguish the two
states someone cared about. The rules below are all scar tissue.

1. **Grep for the terminal line, not the exit.** For Rust, confirm the literal
   `test result:` line. "The command finished" is not "the tests passed".
2. **Never pipe a long test run through `tail`/`head`/`grep`.** You get the
   *pipe's* exit code, not the command's — a killed run reports success.
   Redirect to a file and check the real status.
3. **Always `timeout`-wrap** `cargo test` / `cargo build`. A deadlock consumes
   ~0% CPU and looks identical to "slow". `timeout` turns a silent 40-minute
   hang into an unambiguous exit 124.
4. **Never poll with `pgrep -f "<pattern>"`.** It matches the watching shell's
   own command line, so it reports finished jobs as running *and* dead
   processes as alive. Check by PID: `ps -p <pid>`.
5. **A static trace is a hypothesis; only a measurement is a finding.** Reading
   the code tells you what *can* happen. Confirm what *did*.
6. **Test a gate in both directions.** A check that has never been observed to
   pass on a healthy input is worthless — one here reported "not granted" for a
   condition it had never actually tested, and aborted healthy runs.
7. **A skip is never a pass.**

---

## When something fails

- **`cargo test` won't launch**: the harness binary needs the Swift dylib —
  prefix with
  `DYLD_FALLBACK_LIBRARY_PATH=/Library/Developer/CommandLineTools/usr/lib/swift-5.5/macosx`.
  `cargo build` and `cargo run` are unaffected.
- **A test fails to *compile* on your OS**: check for a `#[cfg(target_os =
  ...)]` mismatch between a test and the helper it drives. That breaks the
  whole test build, not one test — and so blocks every push through the hook.
- **The gate is green locally but red for someone else**: check Node. The repo
  currently pins no version (see the open issue on this), and some tests need
  Node 22+.
- **Results look impossible**: make sure nothing switched branches under you.
  Run the gate from a dedicated `git worktree`, not a working copy other
  sessions may `git checkout` mid-run.

---

## Live suites

Only when you specifically need them, and each is documented in
`docs/TESTING.md`:

- `scripts/rc-live-suite.sh` — the 32-case remote-control matrix against a real
  TextEdit window.
- `scripts/verify-no-black-frame.mjs` / `-native.sh` — the hard product rule
  that a share must never flash black. These sample **rendered pixels**; an
  event-level assertion cannot tell "held the frame" from "went black quietly".
- `scripts/release-smoke.sh`, `scripts/verify-universal-app.sh` — release
  artifact checks.
- `scripts/verify-audio-both-ways.sh`, `verify-speaker-playout.sh` — audio
  paths, which need a real audio-enabled run (agent launches set
  `PETAL_DISABLE_AUDIO=1`).

Every live run must clean up after itself, **including on the failure path** —
it launches GUI apps that outlive the shell that started them. Verify by PID
afterwards regardless.
