# Pre-release testing guide

**What an agent can prove alone, what still needs a human, and what must be true
before an auto-update is published.**

Written 2026-08-21 after a session that merged twelve P0 fixes and a holistic
review that found four release blockers no single-lane review could see. It is
organised by *what a check can actually prove*, because the recurring failure in
this project is not a missing test — it is a test that passes while the thing it
names is broken.

Companions: `docs/TESTING.md` (tiers + every `PETAL_*` env var),
`docs/VALIDATION.md` (which harness answers which question),
`internal/docs/COCKPIT_RUNBOOK.md` (how to actually re-run the cockpit).

---

## Tier 0 — the automated gate (agent, no display)

`scripts/ci-local.sh`. Around twenty steps (the script is the authority):
version lockstep, release-script unit tests, source provenance, the harness
contracts, frontend check + tests + build, backend typecheck + tests, Rust
default-feature **and** cockpit-privileged builds and lib tests, `cargo build
--examples`, `apps/desktop/vendor/tauri-nspanel` tests, the `scripts/probes/`
clang syntax checks, web-harness build + tests + isolated-deploy simulation,
the **web** no-black-frame pixel gate, a remote-control loopback check, the
docs-site build, and the public-tree PII/secrets scan.

**Proves:** nothing compiles broken, ~2000 Rust unit tests and ~1400 web
tests (desktop frontend + browser client) hold, the wire contracts stay in
lockstep, and the *web* renderer does not go black across a forced gap.

**Does NOT prove** (this is the part that matters):

| Gap | Why |
|---|---|
| Native pixels | `scripts/verify-no-black-frame-native.sh` is **not invoked** by the gate. `ci-local.sh` only compile-checks the probes (`cargo build --locked --examples`) and runs the *web* gate. |
| The updater's cross-volume fix | `install_across_a_real_volume_boundary` early-returns as a **pass** unless `PETAL_TEST_CROSS_VOLUME_DIR` is set. That variable appears nowhere in `ci-local.sh`, `docs/TESTING.md`, or CI. A silent skip reads as coverage. |
| Anything on the wire | No LiveKit peer is involved. Encoder behaviour, SFU reaction to a size change, real decode — all untouched. |
| That a helper is *called* | Many guards are source-regex or pure-helper tests. They prove a function is correct given inputs, not that anything calls it with the right ones from the real path. |

**Rules learned the hard way:**
- Never run the gate beside a live `tauri dev` in the same checkout — it
  regenerates `.svelte-kit` under vite and the app serves 500s.
- Never run two gates at once. `ci-local.sh` includes harness tests that kill
  "owned" processes, so concurrent runs sweep each other — and CPU contention
  makes timing-sensitive tests fail on innocent diffs. Three false failures in
  one day were traced to exactly this.
- Read the log's own `test result:` line, not a wrapper's exit code.

---

## Tier 1 — what an agent CAN do alone on this machine

### 1a. The Test Cockpit — the only automated end-to-end tier

Real native↔web media through prod LiveKit, headless, verdicts in JSON.

```bash
cd apps/desktop
scripts/build-cockpit-primary.sh          # MUST go through this (provenance gate)
cd ..
# Refuse to start if a foreign Petal is running -- do NOT let dev.sh clear it
bash scripts/petal-instance-guard.sh || exit 3
RUST_LOG=info ./apps/desktop/src-tauri/target/debug/desktop --test-case=quick \
  > /tmp/cockpit.log 2>&1 &
D=$(ls -dt ~/Library/Logs/Petal/test-runs/*/ | head -1)
python3 -c "import json;[print(o['payload']['scenarioId'],o['payload']['verdict']) \
  for l in open('${D}run.jsonl') if (o:=json.loads(l)).get('kind')=='scenario-verdict']"
```

Expected green: `SHARE-N2W-Q`, `SHARE-W2N-Q` (29–31fps), `DRAW-N`, `CAM`
(~19–22fps), `AUD`, `TELE`.

⚠️ The runbook's own recipe starts with a bare `pkill -f "target/debug/desktop"`.
**Do not run that line** on a shared machine — it kills other sessions' live
instances. Use the instance guard.

**Proves:** media flows both directions at real frame rates, camera publishes,
audio decodes to audible PCM, telepointer and drawing round-trip.
**Does not prove:** anything about a *second native peer*, device changes,
update installs, or window-level input scoping.

### 1b. Native pixel gates — the never-black-frame rule

```bash
bash scripts/verify-no-black-frame-native.sh        # both directions
# and the #840 scenario specifically:
#   --retire-reuse  (retire → reuse-reveal holds the retained frame)
```
Needs a real display and Screen Recording. **Read the transcript, not the exit
code** — the script exits 0 on a harness-invalid skip.

Honest limit: the `--retire-reuse` probe order-outs and order-fronts a bare
`AVSampleBufferDisplayLayer`. It proves the *mechanism* (the layer retains
pixels) and **not the wiring** — it never calls `compositor.rs`'s
`ensure_window` reuse branch. Treat a pass as necessary, not sufficient.

### 1c. Targeted probes (`apps/desktop/src-tauri/examples/`)

`audio_probe`, `mic_capture_probe`, `camera_cadence_probe`, `capture_probe`,
`compositor_probe`, `event_tap_probe`, `hold_last_frame_probe`,
`frontmost_probe`, `bare_window_probe`. Plus `scripts/verify-audio-both-ways.sh`,
`verify-speaker-playout.sh`, `verify-rc-window-identity.sh`,
`verify-receiver-render.mjs`, `verify-t0-battery.sh`.

### 1d. The cross-volume updater test — must be run deliberately

```bash
DEV=$(hdiutil attach -nomount ram://262144 | tr -d ' ')
diskutil eraseVolume APFS PETALXDEV "$DEV"
# confirm the device ids differ, or the test proves nothing:
stat -f '%d' /Volumes/PETALXDEV; stat -f '%d' "${TMPDIR:-/tmp}"
PETAL_TEST_CROSS_VOLUME_DIR=/Volumes/PETALXDEV cargo test --lib updater
hdiutil detach "$DEV"
```
Verified working 2026-08-21: with the fix, 13/13 pass; staging in the system
temp dir instead reproduces the user's exact `code: 18, kind: CrossesDevices`.

### 1e. Telemetry verification — read the pipes, don't assume

- **Sentry:** query the project API for events by release and time window.
  Check new diagnostics arrive **titled**, not `<unlabeled event>`.
- **PostHog:** confirm events exist for the build under test. Note builds older
  than 2026-08-17 send **nothing** — absence is not evidence about them.

### What an agent must NOT do here

- Drive the GUI with computer-use to "verify" — it steals focus and hides the
  user's apps. Use CLI probes and log reads.
- Run `npm run dev:clean` while any other Petal is alive; it kills them.
- Leave scratch volumes mounted or target apps running after a test.

---

## Tier 2 — what a HUMAN must do

These cannot be reached from this machine by an agent: they need a second
person, a second device, real hardware events, or a judgement about how
something *feels*.

### H1. The update itself — the one that gates everything

**Why first:** an update that cannot install makes every other fix
undeliverable. On 2026-08-21 a real user clicked install twice, 45 minutes
apart, and failed both times.

1. Install the **previous** public build. Launch it. Accept the update prompt.
2. Confirm it installs, relaunches, and reports the new version.
3. Repeat with the app in a **non-standard location** (`~/Applications`, or an
   external volume) — that is the `EXDEV` case.
4. Confirm the app survives a **failed** install: interrupt it, or decline the
   admin prompt, and check Petal is still there and still launches.

**Known limit, state it plainly:** users on old builds run the *old* installer
code. This fix protects updates **from this release onward**. Anyone already
stuck needs a direct download link.

### H2. Audio device change mid-call (#867) — cheapest high-value check

Join a call with audio. **Switch System Settings → Sound → Output** (or unplug
headphones) mid-call.

**Pass:** audio follows the new device, and the log records a re-point.
**If nothing happens, the fix does not work on macOS** — a review traced the
default-device lookup as Windows-only, with macOS falling through to
enumeration order. One minute to check, and it decides whether that lane
shipped anything real.

### H3. Remote control, both directions (#759) — never skip the allow case

Two windows of the same app, share only one, grant a remote peer control.
1. Peer clicks the **unshared sibling** → must be refused.
2. Peer clicks and **types into the authorized window** → must still work.
3. Drag from inside the shared window to outside it → nothing should land in a
   sibling.

**(2) is not optional.** A previous fix here refused **284 real key events** in
a live session because only the blocked case was ever verified. A guard that
refuses everything converts a security hole into a broken feature.

### H4. The stuck overlay (#872) — the desktop-wide one

Share a window, **turn on drawing/annotation**, then end the share by every
path that is not the hover tab: stop from the session UI, leave the room, let a
reconnect happen, have the peer disconnect.

**Pass:** after each, you can still click other applications normally, and no
small floating share handle remains. **Fail looks like:** an unclickable region
where the share used to be — often with a tiny handle visible.

### H5. Camera recovery on a receiving peer (#866)

With a second peer watching your camera tile, force a mid-meeting camera
renegotiation (the menubar popover rows were the field trigger).
**Pass:** the tile resumes within ~1s, renders at the new size, and **never
goes black**. Check both a native and a web receiver.

### H6. Share stability under a demand change (#841 / #869)

Share a **display** to a peer whose window forces a quality cap, then resize
that peer's window across a rung.
**Pass:** the sharer's log settles at ~2 republishes — not a storm — and video
keeps flowing during the rate-limit window rather than freezing for ~3s.

### H7. Web→native audio (#787)

One peer on **web** with mic on, one on **native macOS**.
**Pass:** the native peer hears the web peer.

Run with `RUST_LOG=info,livekit=info,webrtc_sys=info`, on a build containing the
log-channel repair — on any earlier build the decisive lines cannot appear at
any `RUST_LOG` setting.

**Then measure, and act on it without discussion:**
```bash
grep -c '\[libwebrtc\]' ~/Library/Logs/Petal/petal.log   # ÷ meeting minutes
```
- single digits/min → fine.
- **hundreds/min → set the denylist to `libwebrtc=error` immediately.**
Rotation bounds *storage*, not *evidence retention*: a flooded log looks healthy
but no longer reaches back far enough to diagnose an intermittent bug — and it
shrinks the 256 KiB tail attached to user feedback reports.

### H8. General feel

Join, share, camera on/off, spotlight, leave, rejoin. Watch for: black flashes
(never acceptable — a frozen frame is fine), truncated UI text (a hard rule),
and a Dock icon that matches its neighbours in size.

---

## Before publishing an auto-update

Every one of these, in order. Do not skip on a green gate alone.

- [ ] **Tier 0 green** — `ci-local.sh`, read the `test result:` lines.
- [ ] **Cockpit Quick tier 6/6** at the expected frame rates.
- [ ] **Native pixel gate** run on a real display, transcript read.
- [ ] **Cross-volume updater test** run with a real second volume.
- [ ] **H1–H7 human checks** done, each result written down — including the
      `[libwebrtc]` rate number.
- [ ] **Version bumped past any version whose recorded evidence predates the
      work.** 0.9.0's recorded evidence ("cockpit 6/6 green") predates ~130
      commits; shipping those under that version string claims coverage that
      does not exist.
- [ ] **Release build verified**: universal (both slices), notarized, stapled,
      `spctl` accepting the DMG **and** the app inside the mounted DMG,
      `PETAL_BACKEND_URL` baked.
- [ ] **A direct download link** published for users who cannot auto-update.
- [ ] **Sentry watched for the first hour** after publish — specifically the
      native-WebRTC line rate and any new untitled events.

### The standing rule

If a check was skipped, say so in the release notes rather than letting a green
summary imply it ran. This project has shipped green and failed live repeatedly;
every instance traced to a signal that could not distinguish the two states it
was read for. **A silently-skipped test is worse than no test, because it reads
as coverage.**
