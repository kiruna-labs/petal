# Validation — how Petal knows whether it works

This is the canonical map from **question** to **harness**: which instrument is
allowed to answer which question, and what is currently unanswered. Its
companion is `docs/TESTING.md`, the how-to-run reference (and
`docs/TESTING-START-HERE.md` if you are new). Read this document
when deciding **"is this evidence enough to close the issue?"** — that decision
has gone wrong here more than 30 confirmed times in one audit (2026-07-28,
issues #621–#624), always in the same way: *a signal was read as distinguishing
two states it cannot distinguish.* The lead exhibit: this repo's own
`docs/TESTING.md` cockpit-status block published "passed 6/6" from a run
artifact containing **no conclusion event at all**, while the real latest run
was a `SHARE-N2N` infra-fail. The document recording whether the system worked
was itself unable to tell.

## The four axes

A validation setup is a point in a four-axis space, each axis running cheap →
real. **The axes are independent** — a harness can be maximally real on one
axis and synthetic on another, and most of ours are (the cockpit is prod
transport + real capture + one host; `camera_cadence_probe` is local transport
+ synthetic source + cross-architecture endpoints).

| Axis | cheap → real |
|---|---|
| **Transport** | none → local SFU (`livekit-server --dev`) → cloud SFU → cloud over real distance |
| **Source** | synthetic frames → test-pattern window → real app window → real user content |
| **Endpoints** | web ⇄ web (control) → web ⇄ native → native ⇄ native → cross-architecture |
| **Hosts** | one machine → two machines |

Notes vs. the naive version of these axes, learned the hard way:

- **A publisher with no subscriber is not a cheap endpoint rung — it is off the
  axis entirely.** `publish_probe` alone measures a pipeline whose dominant
  cost (the receive-side jitter buffer: **131.6 of 138 ms** measured on real
  capture) does not exist. It reported ~20 ms flat with a ~0 ms jitter buffer
  across every simulcast ladder; two rounds of ladder conclusions drawn from it
  inverted on real capture hours later (`internal/docs/COURSE_CORRECTION.md`, amended
  fast-probes rule). A lower layer does not even *bound* the real number.
- **Web ⇄ web is a control, not "the easy tier".** It contains zero Petal
  native code; a defect reproducing there is not ours. Its value is
  discriminating "our pipeline" from "the media server / the browser".
- **Cross-architecture does not require two hosts.** The universal build's
  x86_64 slice under Rosetta 2 runs the real x86_64 code path (SIMD selection,
  codec/pixel-format branches) on this machine. It does not exercise Intel
  silicon/GPU/hardware encoders, and its timings are pessimistic — a defect
  that reproduces is real; one that doesn't is weak evidence, say so.
- **The two-host end of the Hosts axis is closed by policy.** There is no
  second Mac and never will be (`internal/docs/COURSE_CORRECTION.md` §3). Everything
  above one host is either re-scoped locally or honestly marked unvalidated.

## The ladder

Layers, what each can answer, and what it structurally cannot. A harness at
layer N may never decide a question that belongs to layer >N (Rule 1 below).

| # | Layer | Answers | Cannot answer |
|---|---|---|---|
| 0 | Pure unit / contract tests | arithmetic; wire-format lockstep across Rust/backend/web | whether anything real calls it, in the right order (see CLAUDE.md's #497 pattern) |
| 1 | Real capture or real UI, no transport | ScreenCaptureKit geometry/content (`capture_probe`); AppKit env (`bare_window_probe`) | anything about publish/subscribe or latency |
| 2 | Real native app as injection target, local transport | remote-control replay on real UI (TextEdit matrix) | anything about video |
| 3 | Local SFU + synthetic frames | wire format, pub/sub plumbing, event ordering a real SFU produces, cross-arch pipeline deltas | **latency, at all** (dominant cost absent — see above); real-capture cadence |
| 4 | Local SFU + real capture | config ranking, per-stage attribution, press-to-photon estimates | absolute production latency; prod server behavior |
| 5 | Cloud SFU, one machine | does the prod server behave like `--dev`; real token/backend path | network distance effects; independent clocks (sender and receiver share one clock and one CPU) |
| 6 | Cloud, two machines, same platform | real clocks, no shared CPU/GPU contention | cross-platform negotiation — **no live harness; closed by policy** |
| 7 | Cloud, two machines, cross-platform | codec/arch negotiation on real hardware | — (**no live harness**; the code-path half is answerable at L3/4 via Rosetta) |
| 8 | L7 + real user content and network | what users actually get | — (**nothing exists here**) |

Windows is a supported second native platform, so tiers 6 and 7 are meaningful
for Windows hardware and Mac ↔ Windows sessions. No cross-platform live harness
is maintained here; those tiers remain validation gaps rather than missing
Windows product functionality.

## Harness map

Every harness/probe/script/gate, its layer, what a pass proves, and what it
cannot prove. "L" = ladder layer above.

| Harness | L | A pass proves | Cannot prove |
|---|---|---|---|
| `cargo test --lib` (default + `cockpit-privileged` configs) | 0 | pure logic given its inputs; contract fixtures (`contracts/petal-contracts.json`) | that the real event/lifecycle path calls it (CLAUDE.md: #376/#466/#497 all shipped green and failed live) |
| `petal-scorecard-gate` | 0 | a scorecard JSON's p95 is under a ceiling (**default 150 ms — stale; real target <100 ms**, CLAUDE.md Known deviations) | anything about how the scorecard was produced |
| `apps/desktop` `npm test` (incl. rendered-pixel toast check, #422) | 0 | frontend units; one real rendered-pixel truncation check via headless CDP | native/media behavior |
| `backend npm test` / `web-harness npm test` | 0 | handler/source correctness; web/native contract pinning | **the live deployment** (deploys are separate manual `vercel --prod` steps) |
| `scripts/ci-local.sh` | 0 | the aggregate of the above, both Rust feature configs, per-config labelled test totals, static loopback inventory | runtime behavior; `--no-default-features`, release builds, the `harness/` crate (its own header says so) |
| `remote-control-local-loopback.mjs --check-only` | 0 | contract + harness inventory still exist; Swift sentinel typechecks | that any replay works |
| `backend npm run test:local` | 3 | token/JWT/room plumbing against a local SFU | media |
| `share_lifecycle_probe` (+ `--late-joiner`) | 3 | pub/sub event ordering a real SFU delivers (e.g. `TrackSubscribed(new)` before `TrackUnpublished(old)`); has in-run positive controls | latency; real capture |
| `startup_layer_probe` | 3 | simulcast startup layer selection/ramp mechanics; `--pin-lowest` positive control | latency, real-capture fps (synthetic source) |
| `audio_probe` / `camera_cadence_probe` | 3 | transport round-trip for synthetic tone / synthetic NV12; cross-arch pipeline deltas (identical input bytes) | device capture; receiver layer-selection policy (probe README says so) |
| `petal-harness` live runner (`--features live-io`) | 3 | synthetic BGRA bots publish, subscriber sees frames, scorecard emitted | impairment (records the `--impairment` label, **applies no shaping**); latency meaning |
| `capture_probe` (`--geometry`) | 1 | real ScreenCaptureKit capture + numeric geometry/content assertions; manufactures #531's failure as control | transport, latency |
| `frontmost_probe` | 1 | foreground-ownership timeline in ms | anything media |
| `bare_window_probe` / `mint_token` | 1/– | AppKit env isolation / token utility | product behavior |
| `publish_probe` + `subscribe_probe` (paired) | 4 | real capture through local SFU with embedded-timestamp latency; **unpaired `publish_probe` is off-ladder** (no subscriber → no jitter buffer) | prod server, absolute latency |
| `compositor_probe` | 4 | decoded frames into the real display-layer type | the real compositor window lifecycle (plain AppKit window; no retire/reveal — the class of gap that hid #416) |
| `rc-live-suite.sh` (30-case matrix) | 2+4 | web controller → local SFU → native replay into real TextEdit; host-side effects | prod path; its "Live status" lines in TESTING.md go stale silently (the 2026-07-05 "19/0/9" was recorded hours before the join path broke, unnoticed for two days) |
| `rc-live-suite.sh --press-to-photon` | 4 | web-input → estimated-browser-display p95 (software `expectedDisplayTime`, not a photodiode) | physical photon time; prod latency |
| `.github/workflows/nightly-loopback.yml` | 2+4 | the `--live` loopback nightly, **only if** a self-hosted TCC-granted Mac runner is registered | anything, when no runner exists (unverified whether one currently does) |
| Autotest socket + `autotest-run.mjs` scenarios | – | driver, not an oracle: state/accessibility preflights | correctness of anything it drives |
| Test Cockpit (`cockpit.mjs`, `test_cockpit/`) | 5 | scenarios against **prod** SFU on one machine; native+headless-web, `SHARE-N2N` via second local binary (`target-peer/`); verdicts carry `evidenceBasis` (`HostEffect`/`ContentVerified` vs. weaker `WireShape`/`LivenessProxy`/`Scaffold`) | two-host effects; `RC-01..06` and `RES-04` are ⛔ Gap in `internal/docs/COCKPIT_TEST_MAP.md`; `RC-P1080` is a narrow smoke, not the 30-case matrix |
| Quick prod cross-client check (fixed test room, TESTING.md) | 5 | manual native receiver + browser test-pattern sharer in a prod room | real user content; the browser share freezes when its tab is backgrounded (watchdog retires the window — a rig artifact, not a product bug) |
| Manual cross-client test (TESTING.md) | 5 | two independently-permissioned real clients, real devices | repeatability; nothing records it |
| `release-smoke.sh` | 5-gate | signed-artifact static assertions (team, rpath, baked backend URL) + human clean-TCC checklist + petal.log marker assertions | that markers came from *this* run — **on `main` at time of writing**; branch `claude/issue622-release-gates` fixes this (see below) |
| `verify-backend-live.sh` / `verify-web-harness-live.sh` | 5 | the **deployed** endpoints serve current behavior (catches "forgot to redeploy") | media |
| Rosetta x86_64 tier (TESTING.md) | 3/4 | the x86_64 *code path* on one host; reproduced defect = real | Intel silicon/GPU/hardware encoders; representative timings |
| `cross-machine-rc-suite.sh` | 6/7 | (by design) the 30-case suite with a genuinely separate SSH-reachable Mac, all four arch pairings, rejects Rosetta | **anything yet — never live-validated** (#79); requires the second Mac that will not exist |
| `web-harness/` | control | a browser peer with zero native code; the standard second peer for every tier | anything about our native pipeline — a defect here is not ours |

### Native clipboard validation boundary

The native clipboard extension has two distinct questions. Layer-0 tests can
prove byte limits, UTF-8/NUL/file rejection, stream-header correlation,
sequence guards, lifecycle clearing, and the absence of clipboard text from
logs/ledgers. The existing macOS AX tests and `rc-live-suite.sh` cases 10/11
prove host-side native Copy/Paste actuation, not cross-machine clipboard
transfer.

A separate OS session or a second machine is required to prove that B's
clipboard is copied to A, that A's changed sequence rejects a delayed Copy
response, and that A's text updates B before native Paste. Two Petal processes
on one desktop share the same OS clipboard: that setup cannot distinguish A
from B and must never be reported as transfer evidence. Browser peers are
likewise not evidence for the native stream; they intentionally ignore the
native-only Copy request/topic.

Petal's keyboard clipboard semantics are fixed at the boundary (Copy B→A,
Paste A→B). A keyboard Copy→Paste is not a supported native/lossless B-local
workflow. Validate B-local behavior, when needed, by using the target
application's reachable in-window context menu, toolbar, or dropdown for both
operations; do not mix that UI Copy with Petal keyboard Paste.

## Rules — each traceable to a real incident

1. **A harness may not decide a question above its layer.** The synthetic
   probes (L3) priced the jitter buffer at "≤10 ms, do not start here"; real
   capture measured **131.6 ms**. That mispricing parked #214 — the dominant
   latency lever — at P3 for three weeks (`internal/docs/COURSE_CORRECTION.md` §4c).
2. **A probe's numbers decide nothing until that probe has reproduced one
   known real-path effect through the same pipe.** Verbatim from the amended
   fast-probes rule: a fast answer from a path the user is not on is not a
   cheap answer, it is a wrong one with a short feedback loop.
3. **A control never observed to fail has not been shown to work.**
   `startup_layer_probe` once divided decoded frames by an absolute timestamp,
   so a healthy 30 fps stream read as "FAIL — #299 reproduces" **while its
   positive control could never fail** (fixed on `main`, `9713f69f`). Inverse
   case: `screencapture -l` silently returns a fully transparent image for our
   nonactivating NSPanels — a known-good component (the share border) reading
   0 px is what exposed the blind instrument before #196 was mis-confirmed.
   Run the control **before** the first real reading, on the same class of
   thing, through the same instrument (`internal/docs/COURSE_CORRECTION.md` §4b).
4. **Zero evidence renders INSUFFICIENT DATA, never a pass.** The cockpit
   reported "no regression" on runs that measured nothing — p95=0 was
   indistinguishable from perfection (#621; fixed on `main`, `e1181d5f`).
   `ci-local.sh` blocks when the test total can't be parsed rather than
   trusting exit codes; `release-smoke.sh` (branch) dies with `INSUFFICIENT
   DATA` when nothing was logged since the run boundary. Gates also **fail
   closed**: the old `otool | grep` rpath check passed when `otool` itself
   errored (empty pipe → no match → "no CLT rpath").
5. **A summary statistic without per-run values is not reportable.** Seven
   identical runs spanned 15.9–92.7 ms (5.8×) behind a single quoted median
   (§4c.3); #288's "80 ms text penalty" was `+312.6 / +66.7 / −3.8` across
   three runs — the effect lived entirely in runs already flagged as
   contaminated. Per-run first; a pooled number that disagrees with the
   per-run table is an artifact.
6. **Liveness is not throughput.** A locked screen leaves every health signal
   green while capture delivers nothing ("stream alive, source not drawing" —
   TESTING.md); the pre-fix release-smoke "frame pump heartbeat" marker was
   satisfiable by one static frame re-pushed forever. Pair every
   alive/connected signal with a produced-output counter, and treat *alive
   with zero output* as a distinct, alarmed state.
7. **A result not posted did not happen; absence of a report means "unknown".**
   Three decisive gate-failing runs sat on disk nine hours while the issue was
   reasoned about as "probably stale" (§4).

## Gaps

### A. Layers with no harness at all

- **L6–L8 (anything two-host).** `cross-machine-rc-suite.sh` exists but has
  never run against a real second Mac (#79) and never will by policy. Real
  clocks, real network distance, and real user content are permanently
  unmeasured; every latency claim is a shared-clock, shared-CPU number.
- **Glass-to-glass at L5+.** Nothing has ever measured latency against the
  real cloud SFU. The only figure (187.2 ms avg, #179) is L4, stale (#613
  lists five pipeline-touching commits since), and 2× over the <100 ms target.
- **The last pipeline stage.** `compositor::push_frame` takes no `frame_id`
  (`compositor.rs:2934`), so no capture-side timestamp can be correlated with
  a compositor present. **Every latency figure ends before the screen and is
  therefore a lower bound.**
- **`RES-04`** (display sleep) has no runnable scenario; **RC-01..06** are ⛔
  Gap in the cockpit journey table.

### B. Harnesses whose layer is lower than people assume when quoting them

This is where the damage came from — the number was real, the question wasn't.

- `publish_probe`/`startup_layer_probe` (L3/off-ladder) quoted for **latency
  and ladder ranking** — two full measurement rounds inverted on real capture.
- The cockpit-status block in TESTING.md (a doc) quoted as a **run verdict** —
  "passed 6/6" from an artifact with no conclusion event.
- `petal-scorecard-gate`'s 150 ms default quoted as **the** target — the real
  target is <100 ms; a green gate at 150 does not mean the product is right.
- `rc-live-suite.sh` "Live status" lines quoted as **current** — they record
  one hand-run moment and rot silently (the 19/0/9 incident).
- `--press-to-photon` quoted as photon time — it is a software
  `expectedDisplayTime` estimate.

### C. Harnesses whose coverage is narrower than their name implies

Distinct failure from B: the harness is at the right layer but covers less of
it than the name says.

- **`ci-local.sh` ran 987 of 1118 tests** for weeks: the `cockpit-privileged`
  module was feature-gated out, so ~131 tests — and the module's *compilation*
  — were skipped while the gate stayed green; code that did not compile passed
  "local CI" (#624; fixed on `main`, both configs now built and tested with
  per-config totals). The exemplar of this category.
- `petal-harness --impairment` records the label, applies no network shaping.
- `RC-P1080` covers one smoke path, not the 30-case matrix its "remote
  control" tag suggests.
- `remote-control-local-loopback.mjs --check-only` (what CI runs) proves the
  contract and inventory *exist*, not that replay works.
- `release-smoke.sh` covers a marker list, not the behaviors between markers;
  markers are only as strong as what emits them (see Rule 6).
- Rosetta tier covers the x86_64 code path, not Intel hardware.

## Currently unvalidated (as of 2026-07-29)

- Everything in Gap A.
- **The #622 release-gate fixes themselves.** Branch
  `claude/issue622-release-gates` (commit `51225e17`) makes `release-smoke.sh`
  assert on evidence produced: a `MovingFrameLiveness` latch in
  `session/share.rs` requiring **5 pushes carrying affirmative changed-content
  evidence** (dirty rects or a changed snapshot hash — a frozen share cannot
  satisfy it), byte-offset run boundaries so `--assert-log` only greps output
  appended after the checklist invocation, `otool` failing closed, and
  `update-testing-status.mjs` refusing to publish a status block without
  completeness evidence. That branch defines what good looks like here — and
  is itself **reviewed, not yet gated** at time of writing. Per Rule 3, its
  gates have not been shown to work until each has been observed to fail on a
  manufactured violation.
- The nightly loopback's runner status (whether a self-hosted Mac runner is
  currently registered) — unverified.
- Whether `--text-primary`'s placeholder token and other Known-deviations
  items affect any visual check — see CLAUDE.md.

## Update 2026-08-14 -- what the RC suite now proves, and the residue

The 30-case suite is live again and CURRENT at **27 pass / 2 fail / 1 skip**
(details + defect table: TESTING.md's Live status log). What that number is
and is not evidence of:

**It IS evidence of** (ladder rungs 2+4): grant negotiation and session
restore across release/re-request cycles, real host-side injection into
TextEdit (typing, Cmd+A/C/V, drag-select via AXSelectedText, vertical scroll
via AX visible range, modifiers, keycodes, release/TTL/revoke semantics,
non-focus-stealing), share stability across the whole run (zero ROI ack
timeouts, zero wedge restarts, zero teardowns), and target-observation latency
p95 within budget.

**It is NOT evidence of** (each was nearly miscited during the session that
produced the number):

- the #804 ROI **abandonment fallback** -- healthy SCK acks every ROI on
  attempt 1, so that path runs only in unit tests;
- the #806 **static-source hold** -- the suite's target never goes silent for
  more than ~9s and the watchdog arms at 45s; the separate hidden-app recipe
  in TESTING.md is the validation for that path;
- anything timing-dependent (the #804 fps/ROI lock race was microseconds
  wide);
- horizontal scroll (#811, the one route that genuinely does not exist) and
  case 30's test-only republish hook (#808 residue).

**Standing lesson, second occurrence:** cases 5 and 22 failed for months on
predicates describing a wire shape (`action: 'down'`) the harness stopped
publishing -- the same class as #455's case-4/case-7 findings. A publish-metric
predicate is an assertion about the HARNESS, not the product; when one times
out, diff it against what `harnessApi.ts` actually publishes before reading it
as a product failure.

## Refs

`docs/TESTING.md` (how to run everything) · `internal/docs/COURSE_CORRECTION.md` (§4b
controls, §4c experiment choice, amended fast-probes rule) ·
`internal/docs/COCKPIT_RUNBOOK.md` · `internal/docs/COCKPIT_TEST_MAP.md` · issues #613 #618 #619
#620 #621 #622 #623 #624 #299 #79.
