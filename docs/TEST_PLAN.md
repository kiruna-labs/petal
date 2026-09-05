# Petal Test Plan — the human release walk, and what automates each step

> **This is the top-level test structure.** It is organized the way a human
> tester walks the product before promoting a release — not by transport
> direction, not by effort tier. The Test Cockpit follows this same structure:
> every **phase** and **journey** below is a cockpit selector
> (`node apps/desktop/scripts/cockpit.mjs speak`, `… speak:web-nat`,
> `… AUD-01`), so you can run exactly one slice — one phase, one journey, one
> direction — instead of always e2e. Journey detail, pass bars, and per-journey
> engineering status live in `internal/docs/COCKPIT_TEST_MAP.md`; this file is
> the map of what to test and how much of it a machine can do today.
>
> Three levels: **Phase** (what a human does next) → **Journey** (one thing
> that must work) → **Case** (a concrete check with a pass bar).

## How to read the coverage column

| mark | meaning |
|---|---|
| 🤖 | automated with a real oracle — a green run is evidence |
| 🟡 | partially automated: one direction only, weaker bar, or scaffold that INFRA-FAILs honestly |
| 👤 | human-only today |
| ⛔ | nothing behind it at all |

A journey is only as trustworthy as its oracle. `docs/VALIDATION.md` is the
authority on what counts as evidence; the repeated failure mode in this repo is
a check that *looks* like coverage and asserts nothing (wire echo instead of
host effect, packet counters instead of decoded PCM, `pgrep` instead of pixels).

---

## Phase 1 · GET IN — install, launch, trust (release builds only)

The phase everything else depends on, and the one with the worst automation
coverage — historically the richest source of release-day P0s (#99 rpath,
unstapled app inside a DMG, 0.8.2 shipping without `PETAL_BACKEND_URL`).

| ID | Journey — as a human would say it | Coverage | Runs via |
|---|---|---|---|
| INST-01 | The DMG mounts, Gatekeeper accepts it, the app launches with no errors | 🟡 static half | `scripts/release-smoke.sh` (signing/notarization/rpath asserts + log-marker checklist); the launch itself is 👤 |
| INST-02 | First run asks for the right permissions, and granting them works | 👤 | release-smoke's clean-TCC checklist — needs a TCC-reset machine, no automation can click those dialogs |
| INST-03 | An existing install auto-updates to this build | 👤 | manual: install previous release, publish candidate to a staging manifest, watch the update. **Gap — no staging updater channel exists** (see Gaps) |
| INST-04 | The web app loads at meet.petal.live without console errors | 🟡 | `scripts/verify-web-harness-live.sh` + `verify-deploy-freshness.sh` — exist but nothing runs them on a cadence |

## Phase 2 · JOIN — get into a meeting

| ID | Journey | Coverage | Runs via |
|---|---|---|---|
| ROOM-01 | I join a named room and everyone's presence is correct | 🟡 | cockpit `ROOM-01` (roster oracle unit-tested; live two-sided read not auto-driven) |
| ROOM-02 | 3+ peers in one room stay consistent | 🤖 | cockpit `ROOM-02` / `MULTI-3` |
| JOIN-03 | I click a `meet.petal.live/<label>/<code>` join link and land in the meeting — including with the app hidden (the hot-mic-no-UI class, #783 check 3) | ⛔ | nothing. Highest-value gap in this phase |
| JOIN-04 | I leave and rejoin, twice, and everything comes back (camera #638, display shares #722) | ⛔ | nothing automated; both known-broken classes have shipped |

## Phase 3 · SPEAK — hear and be heard

The biggest product blind spot (#787 was P0 precisely because nothing here was
real). The cockpit's AUD verdict is now decoded-PCM-energy, not packet
counters — but note `cockpit.mjs` defaults `PETAL_DISABLE_AUDIO=1`, so an
audio journey must be run with audio explicitly enabled.

| ID | Journey | Coverage | Runs via |
|---|---|---|---|
| AUD-01 | A web peer speaks → the native app actually **hears** it (decoded PCM carries energy, snippet artifact a human can play) | 🤖 | cockpit `AUD-01` with `PETAL_DISABLE_AUDIO=0`. **PASSES live 2026-08-15** (`kbps=58.8, peak_abs=4984`) after fixing the harness's suspended-AudioContext tone (headless Chrome publishes silence without `--autoplay-policy` + `resume()`). A controlled `red:true` run also passed, refuting #787's RED hypothesis — the incident narrows to the ADM playout leg, which this oracle deliberately cannot see (the WAV tap is pre-ADM) |
| AUD-04 | The native mic speaks → the **web** peer hears it (RMS on the received track, not packet counters) | ⛔ | nothing — the reverse leg of #787, still unvalidated |
| AUD-02 | Mute/unmute round-trips | 🤖 | cockpit `AUD-02` |
| AUD-03 | Audio survives a device swap mid-call (AirPods connect) | 🟡 | cockpit `AUD-03` / `CHAOS-DEVICE` |

## Phase 4 · SEE — webcam feeds

| ID | Journey | Coverage | Runs via |
|---|---|---|---|
| CAM-01 | A web peer's camera shows on native | 🤖 | cockpit `CAM-01` |
| CAM-05 | The native camera shows on the **web** peer | 🟡 | cockpit `CAM-N2W` (#815) — the web viewer reads back the tile's PIXELS (advancing, non-black, changing) behind a canvas positive control, never "a track was subscribed". 🟡 not 🤖 because on a machine with no camera the input is a synthetic NV12 pattern (`PETAL_CAMERA_SYNTH_SOURCE=1`): everything from the publish path onward is proven, AVFoundation CAPTURE itself is not |
| CAM-02 | Camera off shows the centered-name tile, not a frozen frame | 🟡 | cockpit `CAM-02` |
| CAM-03 / CAM-04 | Bitrate tracks quality tier; a frozen camera is detected | 🟡 | cockpit `CAM-03`/`CAM-04` (oracles unit-tested, live telemetry not auto-driven) |

## Phase 5 · SHARE — windows and desktops, both ways

The strongest phase, hardened further by the 2026-08-14 session (#804 #806
#807 — see TESTING.md's live-status log).

| ID | Journey | Coverage | Runs via |
|---|---|---|---|
| SHARE-01 | A shared window appears on the peer as a **real, movable native window** — the defining feature | 🤖 | cockpit `SHARE-01` / `SHARE-N2N`. **First physical run PASSED 2026-08-14** (run 1786746104499): receiver window moved (120,60) with size preserved, sharer window independent — WindowServer geometry oracle on a real two-instance share |
| SHARE-02 / SHARE-03 | Shared content is crisp, smooth, fast | 🤖 | cockpit `SHARE-02`/`SHARE-03` |
| SHARE-07 | Start/stop/close/re-share is clean, no crash, no orphan | 🟡 | cockpit `SHARE-07`; **hiding the shared app is NOT covered and is broken (#810)** |
| SHARE-08 | An occluded/background window still delivers | 🟡 | cockpit `SHARE-08`; the genuinely-static source case is a separate recipe (TESTING.md, #806) |
| SHARE-10 | I can share a whole display | 🟡 scaffold | cockpit `SHARE-10` |
| SHARE-05 / SHARE-06 | Several windows at once; across displays | 🟡 scaffold | cockpit `SHARE-05`/`SHARE-06` |
| SHARE-04 / SHARE-09 | No stall over 10 min; recovers from bad network | 🤖 | cockpit soak/`RES-01` |
| — | Never a black frame, pixel-sampled | 🤖 | `scripts/verify-no-black-frame.mjs` + `-native.sh` — run these whenever capture/compositor code changes |

## Phase 6 · CONTROL — remote control, both ways

| ID | Journey | Coverage | Runs via |
|---|---|---|---|
| RC-01..04 | Click / drag-select / type / shortcuts land in the real target app | 🤖 | `scripts/rc-live-suite.sh` — 30 cases, host-side-effect oracles. **Current: 27/2/1** (fails: #811 horizontal scroll, case 30; skip: 2nd display). The cockpit's `RC-P1080` is a narrow smoke only |
| RC-05 | Control feels instant (<100ms goal) | 🟡 | `scripts/rc-live-suite.sh --press-to-photon` — exists, not run on a cadence |
| RC-06 | Control lands at a scaled share tier | 🟡 | suite case + `RC-P1080` |
| RC-07 | A web peer controls **my** window while I keep working — focus never stolen | 🤖 | suite case 27 |
| RC-08 | Native controls a web/native peer (reverse + nat↔nat directions) | 🟡 | cockpit `RC-N2N` (nat↔nat, host-effect oracles) + `RC-N2W` (nat→web, delivery only — a browser cannot inject OS input). #819. Opt-in tier: RC-N2N needs the test-peer's Accessibility grant, so it never joins a headless sweep |

## Phase 7 · POINT & DRAW

| ID | Journey | Coverage | Runs via |
|---|---|---|---|
| PTR-01 | Peers see my labeled telepointer move | 🤖 | cockpit `PTR-01` |
| PTR-02 | Drawn strokes show, both directions | 🟡 | cockpit `PTR-02` (native→web only today) |

## Phase 8 · SURVIVE — entropy a real meeting produces

| ID | Journey | Coverage | Runs via |
|---|---|---|---|
| RES-01 | Bad network degrades gracefully and recovers | 🤖 | cockpit `RES-01` |
| RES-02 / RES-03 | Device vanishes / display config changes | 🤖 | cockpit `RES-02`/`RES-03` |
| RES-04 | Receiver's display sleeps then wakes (the WindowServer-kill class #264) | ⛔ | no runnable scenario at all |
| RES-05 | A peer leaves abruptly — windows retire, no orphans | 🤖 | cockpit `RES-05` |
| RES-06 | Sharer hides the shared app for 5+ min, then unhides | ⛔ | known broken (#810); the static-source recipe in TESTING.md exercises the first half |
| RES-07 | Sharer's Mac sleeps (lid close) mid-meeting and wakes | 👤 | manual; resilience wiring exists (#734, #749) but no scenario drives it |

## Phase 9 · LOOK — the UI itself

| ID | Journey | Coverage | Runs via |
|---|---|---|---|
| UI-01..04 | Main window / gallery / menubar pill / Dock render correctly, **no truncated text ever** (hard rule) | 🟡 scaffold | cockpit `UI-01..04` — text-overflow oracle unit-tested, live screenshot drive not wired |

---

## Release promotion = one pass through all nine phases

Minimum bar before promoting a build, in phase order (fail fast — a phase-1
failure makes the rest moot):

```
1. scripts/release-smoke.sh                                # GET IN (static)
   + human: DMG open, first-run TCC, update-from-previous  # GET IN (👤)
2. cockpit: join                                           # JOIN
3. cockpit: speak            (PETAL_DISABLE_AUDIO=0)       # SPEAK
4. cockpit: see                                            # SEE
5. cockpit: share  + verify-no-black-frame gates           # SHARE
6. scripts/rc-live-suite.sh                                # CONTROL
7. cockpit: point                                          # POINT
8. cockpit: survive                                        # SURVIVE
9. cockpit: look                                           # LOOK
```

A journey marked 👤 or ⛔ that a release depends on must be walked by a human
or explicitly waived in the release notes — silence is not a waiver.

## Modular runs — the selector grammar

The cockpit resolves, case-insensitively:

- a **phase**: `join`, `speak`, `see`, `share`, `control`, `point`, `survive`, `look`
- a **journey id**: `AUD-01`, `SHARE-01`, …
- a **feature** (legacy axis): `audio`, `screen-sharing`, `A`…`H`
- a **priority / depth**: `p0`, `short`
- a **direction**: `web-nat`, `nat-web`, `nat-nat`, `nat-local`
- an **intersection** with `:` — `speak:web-nat` (audio one way), `p0:short`,
  `share:nat-web`
- an explicit comma list: `AUD-01,CAM-01`
- the legacy tiers: `quick`, `full`, `soak`

Examples:

```sh
node apps/desktop/scripts/cockpit.mjs speak:web-nat   # just "can I hear them"
node apps/desktop/scripts/cockpit.mjs share           # the whole SHARE phase
node apps/desktop/scripts/cockpit.mjs AUD-02          # one journey
```

## The road to 100% — what must happen, in order

"100%" means: **one command, run unattended after any change, that either says
"promote" or names the exact broken journey — across all nine phases — with an
oracle behind every claim.** As of 2026-08-15 we are at roughly half the walk
(share + control deep; join/see/speak/point one-directional; get-in/survive/
look uncovered). The distance, in dependency order:

### 1. Make the covered half honestly green (product fixes)

- **#820 — the post-reconnect control break.** The stale-disconnect revoke is
  fixed (roster-verified, grace-confirmed — measured surviving 2 aftershocks
  per run); the residue is (a) post-resume input refused
  `reason=auth detail=no-active-request` with grants intact — something else
  in the resume path invalidates session state — and (b) replayed ops' terminal
  results never reaching the controller. Fixing this makes the RC suite
  29/30 with only the second-display skip. *The last real product bug between
  the suite and green.*
- **#787's remaining leg** — decode is proven fine (both RED arms, live vs
  prod); the silence is post-decode in the speaker plumbing (ADM playout
  ordering/retry/loudness). The 40-second `speak:web-nat` gate is the
  regression check once fixed.

### 2. Make "green" machine-decidable (semantics, not new infra)

- **Expected-failures allowlist** for `rc-live-suite.sh`: exit code alone
  cannot distinguish known residue from regression (documented caveat,
  TESTING.md). A checked-in allowlist consumed by the runner — failing cases
  outside it fail the run, cases inside it report as `known` — makes the suite
  a trustworthy exit code again. Delete entries as bugs close; an empty file
  is the goal state.
- **One flake policy**: cases 7/10/14 each failed exactly once across five
  loaded runs. Give the numbered matrix ONE bounded retry per failed case
  (like case 29's click retry) and record `flaky:true` on the result instead
  of failing the run — visible, never silent.
- **Fix the feature-clobber structurally** (TESTING.md hiccup 10 — four hits
  in one day): either point `build-cockpit-primary.sh` at its own
  `CARGO_TARGET_DIR` (like the test-peer's `target-peer/`) so pushes can't
  destroy it, or make the cockpit launcher refuse a binary whose feature stamp
  is missing *with the hiccup-10 message* instead of hanging silently.

### 3. Close the coverage gaps (all filed, all scoped — build in this order)

1. **#812 AUD-04** — native mic → web listener (the only P0 feature with an
   unvalidated direction), + keep the audio-enabled SPEAK run in the walk.
2. **#813 JOIN-03** — the join link, including the hidden-app hot-mic ordering.
3. **#814 JOIN-04** — leave & rejoin ×2 (pins the shipped #638/#722 classes;
   its display-share half must start RED against still-open #722).
4. ~~**#815 CAM-05** — native camera on a web tile, frame-advance oracle.~~ Built: cockpit `CAM-N2W`. Live run still owed.
5. **#816 RES-04** — display sleep/wake at the notification boundary.
6. **#817 UI-01..04** — drive the truncation oracles to real screenshots.
7. **#819 RC-07** — native-as-controller on the existing test-peer rig.
   Built: `RC-N2N`/`RC-N2W`, unit-gated, awaiting its first live run on the
   TCC-granted Mac.
8. **#818 INST-03** — staging updater channel; until it exists, GET-IN stays a
   human phase and every promotion must say so out loud.

### 4. One-command release walk (the finish line)

A `release-walk` orchestrator that runs the phase sequence from this file —
gate → suite (with allowlist) → cockpit phases → black-frame gates → the
targeted checks — resumable per phase, emitting ONE verdict table shaped like
the human checklist, with 👤 items printed as explicit TODOs rather than
silently skipped. Prereqs: items 2 (else the verdict lies) and the slot-gated
detached-runner pattern promoted from session scratch into `scripts/`
(TESTING.md hiccup 6 documents the pattern; the script should live here).

### Standing hygiene (cheap, keeps the system trustworthy)

- Re-run the cockpit `quick` tier and refresh `baseline.json` after any
  media-path change (last full-tier green: 2026-08-15, 6/6 incl. one DRAW-N
  retry-flake; before that 2026-07-12 — a five-week silent gap of exactly the
  kind TESTING.md's live-status lesson warns about).
- Deploy freshness: a cockpit-vs-prod run is only as current as
  meet.petal.live; `verify-deploy-freshness.sh` before citing one.
- Every new journey ships with its oracle's mutation check, and TEST_PLAN's
  coverage column moves in the same commit — the map must never flatter the
  territory.

## Audio, both directions — `scripts/verify-audio-both-ways.sh`

One command, two legs, both measured on the **decoded waveform**:

| leg | oracle | result 2026-08-15 |
|---|---|---|
| native → native | `examples/audio_probe subscribe` decodes the PCM and Goertzel-checks 440Hz | RMS 11571, 440Hz dominant |
| native → web | cockpit `AUD-N2W` (journey AUD-04) measures a recorded waveform in a real browser | rms 0.3528 over 4.0s decoded |

Web → native is covered by `AUD-01` (native-side decoded PCM energy). Two rules
this script exists to enforce, both learned the hard way (#821):

- **Never conclude "silence" from a receiver that cannot decode.** Headless
  Chrome does not decode remote audio at all; its counters look healthy the
  whole time. The oracle now throws INFRA rather than reporting `ok: false`
  when packets arrive and zero samples are decoded.
- **Unmute first.** Petal joins muted; a rig that only publishes measures
  correct-but-useless silence.

## The gap list (each has an actionable issue)

Ordered by how big the blind spot is:

1. **AUD-04 — native→web voice** + one audio-enabled AUD-01 run. The only P0
   feature with an unvalidated direction.
2. **JOIN-03 — join links**, including the hidden-app case.
3. **SHARE-01 physical run** — everything is built; one TCC-granted execution
   promotes the defining feature to 🤖.
4. **CAM-05 — native→web camera.** Scenario `CAM-N2W` exists and is unit-gated; the live run that promotes it to 🤖 has not happened yet.
5. **JOIN-04 — leave/rejoin ×2** (pins #638/#722 regressions).
6. **RES-04 — display sleep/wake** (no scenario exists).
7. **RES-06 — hide/unhide the shared app** (blocked by #810's fix).
8. **UI-01..04 live drive** (oracles exist; screenshot orchestration doesn't).
9. **INST-03 — staging updater channel** so update-from-previous is testable
   before the manifest goes live.
10. **RC-08 — reverse/native-native control directions.**
