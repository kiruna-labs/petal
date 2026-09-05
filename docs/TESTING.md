# Testing

> **New here? Read `docs/TESTING-START-HERE.md` first** — the short orientation
> guide: the one command to run, what each gate step proves, the cost tiers,
> and how to read a result without fooling yourself. This file is the deep
> reference you come back to once you know what you are looking for.

> **This file is the how-to-run companion.** For *which harness answers which
> question* — the validation ladder, the evidence rules, and what is currently
> unvalidated — read **`docs/VALIDATION.md`** first, especially before deciding
> whether a test result is enough evidence to close an issue.

Petal has three main automated test tiers plus live diagnostic probes. For the full local gate, run:

```sh
./scripts/ci-local.sh
```

**Platform coverage:** the frontend static suite (`npm test`, `tests/*.test.ts`)
runs on any OS and includes Windows-specific coverage
(`platformTheming.test.ts`, `windowsBootstrap.test.ts`,
`windowsWindowSharing.test.ts`, `contextMenu.test.ts`,
`remoteWindowHeader.test.ts`). Rust tests are per-OS — the Windows-gated tests
(`pe_machine_type_*` in `updater.rs`, `windows_log_dir_*` in `logging.rs`) only
compile and run under a Windows `cargo test`, which now happens automatically:
`.github/workflows/rust-gate.yml`'s `windows` job (`windows-latest`) runs
`cargo build --locked` and `cargo test --lib --locked` on every PR that
touches `apps/desktop/src-tauri/**`, so those two modules — and the rest of
the Windows native surface (`windows_compositor.rs`,
`windows_screen_capture.rs`, `windows_capture_target.rs`, etc.) — are compiled
and exercised in CI, not only when someone happens to run `cargo test` by hand
on a Windows machine (#673). The Windows native product surface is complete
for the current feature set. Live application probes in this file remain
platform-specific: macOS owns the existing remote-control loopback and Test
Cockpit flows, while Windows build/test coverage comes from the Windows CI job
and host integration is covered by the Windows live matrix below.

**`scripts/ci-local.sh`'s coverage limit:** it runs on macOS only (the primary
local gate), so it can never execute the Windows-gated Rust tests above or
compile the Windows native surface — a green `ci-local.sh` run says nothing
about Windows correctness. That gap is covered only by the `windows` job in
`rust-gate.yml` described above (or by a real Windows `cargo build`/`cargo
test --lib`), not by anything `ci-local.sh` runs locally.

### Windows live matrix

Automated Windows Rust/contract tests pin capability composition, keyboard
scan-code identity, held-input cleanup, and one-surface telepointer selection.
They do **not** prove a real application accepted `SendInput` or that native
z-order clipped a rendered name pill. Before calling a Windows remote-control
change live-verified, run this on two local peers (native + browser is enough)
and record each cell as **effect observed** or an explicit terminal refusal:

1. Share Notepad, Edge/WebView2, VS Code, Calculator, and the Tk fixture. For
   each, exercise left/middle/right click, down/up, drag, vertical/horizontal
   wheel scrolling, keyboard, and Unicode text. For Notepad, verify scrolling
   changes the document position **and leaves remote control active**. Repeat
   with the target covered, minimized, closed, foreground-switched, elevated,
   and focused on a password/untrusted-provider field. Every refused/no-op
   operation must produce one brief controller warning and leave the same
   control grant active; uncover/restore the target and prove the next valid
   operation works without requesting control again.
2. On matched controller/host layouts, exercise letters/digits/punctuation and
   shifted symbols, Escape, navigation, F1–F24, both sides of modifiers,
   Caps/Num/Scroll Lock, every numpad position, repeat, AltGr, international
   positions, and dead-key/IME committed text. Win/Meta on a window share and
   browser/OS-reserved keys must report explicit unsupported/unobservable
   results, never count as successful replay.
3. Overlap two locally shared windows and place the sharer's cursor in the
   intersection; only the raised source's corresponding remote surface may show
   it. Then overlap two remote compositor windows and repeat from the viewer;
   only the raised remote surface's corresponding sharer surface may show it.
   Repeat with local/remote ID collisions and with each side alternately raised.
4. Partially and fully cover the selected shared/remote window with an ordinary
   window. Capture the case where the arrow point is exposed but the name pill
   crosses the boundary: native clipping must keep the complete tag off the
   occluder's pixels. Cover/uncover must not leave a stale duplicate, activate a
   click-through overlay, or expose remote-control input over the occluder.
   The current global route must refuse a covered point without falling back to
   another native API, briefly show `Covered`/`Input ignored`, and remain active.
5. Separately verify true lifecycle termination: host disable/revoke,
   controller release/disconnect, share stop/replacement, and room teardown must
   deactivate forwarding and release every held key/button. Do not count an
   operation warning as lifecycle termination.

Use an unobstructed visible tag and a known working Notepad key/click as positive
controls **before** the first negative reading. Packet/log shape alone is not a
host-effect result; record a screenshot or target-side effect for every live
claim.

That script runs desktop frontend checks/build, backend typecheck/tests, and web-harness build/tests. Its Rust gate verifies both of these configurations:

- Default features: `cargo build --locked` and `cargo test --lib --locked`.
- Internal privileged cockpit: `cargo build --locked --features cockpit-privileged` and `cargo test --lib --locked --features cockpit-privileged`.

The test output prints a separately labelled total for each configuration. This
means the privileged cockpit module is compiled and its lib tests run, while the
normal customer-build configuration remains independently covered. The gate
does not build a customer artifact with privileged cockpit enabled, and it does
not run a separate `autotest` configuration: `autotest` gates no additional
tests because its test-relevant code is also enabled by `debug_assertions` in
the default debug test build. It also does not cover `--no-default-features`,
release builds, or the separate harness crate. It is the local gate to run
before pushing.

It also runs the CI-safe remote-control local-loopback hook:

```sh
cd apps/desktop
npm run autotest:remote-control-loopback:check
```

This is intentionally `--check-only`: it proves the remote-control contract and
harness inventory still exist, while the real CGEvent/TextEdit loopback remains
a live Mac + Accessibility gate. On macOS it is also fail-closed: the preflight
runs `xcrun swiftc -typecheck` on the AppKit photon sentinel. Ubuntu CI uses
the explicit portability-only form below; it runs every contract/inventory
check but defers only that macOS compiler probe to the named macOS CI gate:

```sh
node apps/desktop/scripts/remote-control-local-loopback.mjs \
  --check-only --skip-swift-typecheck
```

Do not use `--skip-swift-typecheck` for local or live validation. The flag is
rejected outside `--check-only` and prints a `DEFERRED` sentinel result rather
than a pass.

`apps/desktop`'s `npm test` includes a real rendered-pixel check of the update
toast (`tests/transientTextTruncation.test.ts`, #422) via a headless Chromium
instance driven directly over CDP (no `playwright` test-runner dependency, just
its browser-download CLI). `npm test`'s `pretest` hook runs `playwright install
chromium-headless-shell` automatically, so a fresh `npm ci` needs one-time
network access to fetch the browser; set `PETAL_CHROME_BIN` to point at an
existing Chrome/Chromium binary to skip the download entirely.

## Tier 1: Rust Unit Tests

Rust unit tests live under `apps/desktop/src-tauri/src/**` and run from the Tauri crate:

```sh
cd apps/desktop/src-tauri
DYLD_FALLBACK_LIBRARY_PATH=/Library/Developer/CommandLineTools/usr/lib/swift-5.5/macosx \
  cargo test --lib --locked
```

The `DYLD_FALLBACK_LIBRARY_PATH` setting is required on this CLT-only macOS setup when the cargo test harness launches and needs Swift runtime dylibs. `scripts/ci-local.sh` uses this exact quirk. Do not use this docs task as a reason to run `cargo build`; the local gate owns that.

The Rust suite includes the shared cross-component contract fixture at `contracts/petal-contracts.json` through `apps/desktop/src-tauri/src/rooms.rs`.

The separate self-evaluation harness crate lives at `apps/desktop/src-tauri/harness/`. It is not part of the app crate's `cargo test --lib`; run it from that directory if you are changing the harness crate itself:

```sh
cd apps/desktop/src-tauri/harness
cargo test --lib
```

The `petal-harness` live runner is opt-in because it links the desktop app's
LiveKit transport. It starts one subscriber plus synthetic publishing bots,
pushes generated BGRA test-pattern frames through
`desktop_lib::transport::RoomConnection`, measures received frame metadata via
`Subscriber`, writes a SPEC §7 scorecard JSON, and exits nonzero if no samples
arrive or p95 exceeds the threshold. It requires an existing LiveKit endpoint
and either `PETAL_BACKEND_URL` or the debug fallback env vars
`LIVEKIT_URL`/`LIVEKIT_API_KEY`/`LIVEKIT_API_SECRET`:

```sh
cargo run --features live-io --bin petal-harness -- \
  --room petal-harness-smoke \
  --publishers 3 \
  --shares-per-bot 1 \
  --duration-secs 30 \
  --impairment perfect \
  --out scorecard.json
```

The runner records the `--impairment` label but does not apply OS/network
shaping yet. Validate the live-IO build without joining a room via:

```sh
cargo build --features live-io --bin petal-harness
```

The CI-safe pure modules and scorecard gate below remain the default harness
slice.

The first CI-safe slice is the pure scorecard gate. It performs no LiveKit or
window I/O; it reads a scorecard JSON and enforces the SPEC §2.3 p95
glass-to-glass ceiling, defaulting to 150ms:

```sh
cargo run --bin petal-scorecard-gate -- \
  --scorecard fixtures/scorecard-pass.json
```

Use `fixtures/scorecard-fail-p95.json` to confirm the gate exits nonzero when
the threshold is breached, or pass `--max-p95-ms <n>` to exercise a tighter
temporary limit.

## Tier 2: Backend Tests

Backend tests live under `backend/test/`.

```sh
cd backend
npm test
```

`npm test` runs both `test/distribution.ts` (mocked Vercel Blob/API fetches; covers the distribution/update endpoints and blob helpers) and `test/privacy.ts` (covers room-credential/access-code derivation, Sentry PII-scrub allowlisting, and admin-auth error contracts).

There is also a live local integration test:

```sh
cd backend
LIVEKIT_URL=ws://localhost:7880 LIVEKIT_API_KEY=devkey LIVEKIT_API_SECRET=secret npm run test:local
```

`npm run test:local` runs `test/local.ts` against `livekit-server --dev`, and verifies slug lockstep, JWT grants, LiveKit admin create/list/delete, and room-directory behavior.

**`npm test` only proves the SOURCE is correct — it never touches the live deployment.** Real incident (2026-07-05): the invite-link route (`api/j.ts`) and a round of copy edits to the join/download pages were correct on `main` and passed `npm test`, but the live backend still served the OLD build because it was never redeployed (`vercel --prod` is a separate, manual step from `git push`). Unit tests against the handler functions cannot catch "forgot to redeploy" — only hitting the actual production URL can.

**After every `cd backend && vercel --prod` deploy, run:**
```sh
scripts/verify-backend-live.sh
```
It curls the LIVE `app.petal.live` (the root redirect to the marketing site, the updater manifest, and `/api/rooms`) and asserts the current expected behavior is actually being served — not just that the routes exist in the repo. Exits non-zero with a clear per-check failure if the deploy is stale or a route regressed. Point it at a different backend with `PETAL_BACKEND_URL=... scripts/verify-backend-live.sh`. The invite-link rewrite shapes (`/:code`, `/:label/:code`) moved to `meet.petal.live` along with the browser SPA — see `scripts/verify-web-harness-live.sh`.

## Tier 3: Web-Harness Tests

The browser harness lives in `web-harness/`; its tests live in `web-harness/tests/`.

```sh
cd web-harness
npm test
```

`npm test` runs `node --test tests/*.test.ts`. These tests pin web/native contracts for meeting-code parsing, track names, telepointer payloads, and remote-control messages. `npm run build` also compiles the app and the tests project.

For live browser testing:

```sh
cd web-harness
LIVEKIT_URL=ws://localhost:7880 LIVEKIT_API_KEY=devkey LIVEKIT_API_SECRET=secret npm run dev
```

The dev server's token middleware reads LiveKit credentials in Node from `apps/desktop/.env` or the process environment. Browser JS receives only `{ url, token, room }`.

**⚠ Running a live harness from a git worktree: `apps/desktop/.env` will be missing.** It is gitignored, so it exists only in the checkout where someone created it — not in any worktree. The failure is badly misleading: the browser peer never joins and you get `web harness connected to native room timed out after 15000ms`, which reads as a *room* problem. The real cause is in `webharness.log`: `[token-endpoint] apps/desktop/.env NOT found (LIVEKIT_URL: MISSING, ...)` — the token endpoint could not mint a token, so the peer never had credentials.

It looks even more like a room problem when the native side joins fine, which it will if you passed `LIVEKIT_URL`/`LIVEKIT_API_KEY`/`LIVEKIT_API_SECRET` directly to the Petal process — Petal is then happy while the browser peer is not.

Fix: create `apps/desktop/.env` in your worktree with the local `livekit-server --dev` credentials (`devkey` / `secret`). Those are the documented dev defaults, not secrets. Confirm it is gitignored before writing — it is, but check rather than assume, since this file holds credentials in other setups.

**⚠ Cold-start race: gate on harness readiness before driving the page.** On a fresh browser profile, vite's cold-start module transform can still be running when a scenario clicks `#join-btn`. The click no-ops, the room never connects, and the suite then fails downstream with timeouts that read as a transport or room problem rather than a page that was not ready.

It is intermittent in the worst possible way: **any manual debugging loads the page first and warms it**, so the suite passes while you are watching it and fails when you are not. One agent lost a run to this and only found it after a clean-profile run behaved differently from its hand-driven one.

Wait for `#join-btn`, `#meeting-code` **and** `window.__petalHarness` to exist before starting the suite, and fail with a **distinct exit code** if the page never becomes ready — so "the page was not up" can never be misread as "the product did not deliver."

**⚠ `screencapture -l <CGWindowID>` returns a fully TRANSPARENT image for every transparent Petal window** — the share border and share overlay (nonactivating NSPanels), **and the MAIN window**, which is `transparent: true` in `tauri.conf.json`. It does not fail. It silently yields **0 non-transparent pixels**, which reads exactly like "the window rendered nothing," and will produce a confident false negative for any test asking whether it painted.

Confirmed again on the main window while validating #636 (measured: alpha 0 across all 1,024,000 pixels, which flattens to a perfect black box). Flattening such a capture to luminance before checking alpha is what turns it into convincing evidence for the wrong conclusion — **check the alpha channel before believing a dark capture.**

One agent nearly concluded #196 (sharer cannot see remote strokes) reproduced on that basis. What exposed it: the **share border** — known-good and definitely rendering — read 0 px too. A positive control on a component you already trust is the only thing that distinguishes "the feature is broken" from "my instrument cannot see it."

Use a **region** capture instead — `screencapture -R x,y,w,h` — with the target window genuinely unoccluded. And note the foreground app re-occludes between steps, so **re-raise and re-verify immediately before every capture**, not once at the start.

### Human-only Safari audio playback check

Safari can block remote audio playback after LiveKit negotiation because the
remote `<audio>` attach happens outside the original Join button gesture. For
web-harness audio changes, run this live browser check in addition to unit tests:

1. Join the same room from Safari and from a second peer that publishes audio.
2. Confirm the web-harness session log includes `track subscribed: ... (audio)`.
   If that line is absent, debug the sender/publish path before treating browser
   autoplay as the cause.
3. If Safari shows **Enable audio**, click it and confirm remote audio is heard.
4. Confirm the prompt disappears after the click and does not reappear while
   audio continues playing.

### Human-only browser device selection check

The web harness exposes audio device selectors in **Developer & test tools**.
Speaker selection is intentionally hidden in browsers without
`HTMLMediaElement.setSinkId()` support, including Safari and Firefox.

1. In a supported browser, choose a non-default microphone, enable the real
   microphone, and confirm a second peer hears that physical input.
2. Change the microphone while live and confirm the peer hears the new input.
3. Choose a non-default speaker in a browser that supports output routing and
   confirm remote audio routes to that device.
4. Reload, rejoin, and confirm the last selected available devices are reused.
5. Unplug the selected device, trigger the browser `devicechange` path, and
   confirm the UI falls back to **System default** instead of failing.
6. In Safari/Firefox, confirm the speaker picker is absent, not shown disabled.

### Retrieving web-harness session logs

The web harness keeps the last ~500 session events in memory, stamped with the
browser participant identity and meeting room. Open **Developer & test tools**,
then click **Download session log** to save
`petal-session-<identity>-<room>-<timestamp>.log`. Each line is:

```text
ts identity room [kind] message
```

To correlate a browser session with the native app, compare the downloaded
web-harness log with `~/Library/Logs/Petal/petal.log` and match entries by room,
identity, and nearby timestamps.

For local desktop GUI joins, app startup loads both `apps/desktop/.env` and
`apps/desktop/src-tauri/.env` without overriding process environment values. In
debug builds, if `PETAL_BACKEND_URL` is unset but `LIVEKIT_URL`,
`LIVEKIT_API_KEY`, and `LIVEKIT_API_SECRET` are present, the native token client
uses a local dev-only mint fallback so `npm run dev:clean` can join rooms before
the Vercel backend is deployed. Release builds still require
`PETAL_BACKEND_URL`.

## Quick single-machine PROD cross-client validation (native ⇄ browser)

The fastest way to get a native receiver + a browser sharer into the **same
prod room** on one Mac — good for validating receiver-side compositor behavior
(remote-window header/chrome, telepointer, remote control) without a second
machine. Uses the deployed `app.petal.live` backend, so no local LiveKit needed.

Use a single fixed test room and reuse it — don't mint new rooms per run, they
persist in `rooms.json`. Set `PETAL_TEST_ACCESS_CODE` in your shell to your own
room's access code; the commands below read it. Never commit a real access code:
it deterministically derives the room credential (see `docs/CONTRACTS.md`), so
publishing one publishes a working join capability.

1. **Native receiver (dev build, auto-joins on launch):**
   ```
   cd apps/desktop
   PETAL_BACKEND_URL=https://app.petal.live \
   PETAL_AUTOTEST_ROOM="$PETAL_TEST_ACCESS_CODE" \
   PETAL_AUTOTEST_IDENTITY="$(uuidgen | tr 'A-Z' 'a-z')" \
   PETAL_AUTOTEST_NAME="Native-Validation" \
   PETAL_DISABLE_AUDIO=1 \
     npm run dev:clean
   ```
   - **GOTCHA:** `PETAL_AUTOTEST_IDENTITY` MUST be a *generated* id — a plain
     lowercase UUID, `web-<uuid>`, or `p-...`. A human name (`native-validation`)
     is rejected by the backend: `identity must be a generated participant id`.
   - Omit `PETAL_AUTOTEST_SHARE` for receiver-only. Confirm the join in
     `~/Library/Logs/Petal/petal.log`: `session: joined room 'room-...'`.
2. **Browser sharer** via `claude-in-chrome` on one of your permissioned
   profiles: navigate to **`https://meet.petal.live/?code=$PETAL_TEST_ACCESS_CODE`** (the
   `?code=` QUERY form works directly against the SPA; the `/label/code` PATH
   form also works here now — it resolves via `api/j.ts`'s native-launch
   interstitial, which redirects into the same SPA as its browser fallback).
   It auto-joins; confirm "connected" + 2 participants (you'll see the
   `Native-Validation` tile).
3. **Share to the native receiver:** open **"Developer & test tools"** → click
   **"Share test pattern"**. This publishes the animated `PETAL WEB HARNESS TEST
   WINDOW` canvas as `petal-window-<id>` (no OS permission prompt). The native
   app creates a real compositor window for it (`compositor: creating remote
   window ...`).
4. **Observe the native remote window:** it's a **borderless Normal-level
   NSPanel**. For a visual, `screencapture -x /tmp/shot.png` (CLI) captures the
   whole desktop including the remote window. For **authoritative geometry**
   (position bugs are invisible in a screenshot — esp. the transparent
   control/pointer overlays), query `CGWindowList` directly instead of eyeballing
   a screenshot — this is the single most useful debugging tool here:
   ```swift
   // /tmp/wins.swift — run: DEVELOPER_DIR=/Library/Developer/CommandLineTools swift /tmp/wins.swift
   import CoreGraphics; import Foundation
   let list = CGWindowListCopyWindowInfo(.optionAll, kCGNullWindowID) as! [[String: Any]]
   for w in list where (w[kCGWindowOwnerName as String] as? String) == "desktop" {
     let n = w[kCGWindowNumber as String] as! Int
     let name = w[kCGWindowName as String] as? String ?? ""
     let b = w[kCGWindowBounds as String] as! [String: Any]
     print("id=\(n) name='\(name)' x=\(Int(b["X"] as! Double)) y=\(Int(b["Y"] as! Double)) w=\(Int(b["Width"] as! Double)) h=\(Int(b["Height"] as! Double))")
   }
   ```
   Window names: panel = `petal-window-<id>`; overlays =
   `remote-window-{control,pointer}-<seg>-<id>`. **Correct layout:** panel at
   `(X, Y)` sized `W × (H+42.5)`; the control + pointer overlays at
   `(X, Y+42.5)` sized `W × H` (they cover only the video area below the 42.5px
   header strip, which the panel's own webview renders — the header is NOT a
   separate window). A chrome window at `x≈0` (screen corner) is the
   `addChildWindow` follow-offset corruption bug (fixed by deferring the overlay
   sync out of the panel's own Moved/Resized handler).

**CRITICAL GOTCHA — the browser share freezes when its tab is not the active,
visible Chrome tab.** The test pattern renders via `requestAnimationFrame`,
which Chrome throttles/pauses for occluded or background tabs. When video frames
stop for ≥30s the native receiver's frozen-window watchdog **retires** the
remote window (`compositor feed: no frames ... for >= 30s; retiring frozen
window`), even though telepointer keeps flowing on its own channel. So on a
single screen you cannot both (a) keep the Petal tab foreground/visible AND
(b) bring the native remote window forward to drag it. Workarounds: keep the
Petal tab the active tab in a **visible, un-occluded** Chrome window positioned
so it doesn't overlap the remote window; or launch Chrome with
`--disable-backgrounding-occluded-windows --disable-background-timer-throttling
--disable-renderer-backgrounding`; or validate drag-tracking from the log
(a drag fires `WindowEvent::Moved` → `sync_chrome_to_panel_frame` repositions
the header/control/pointer panels — see #156). A true two-Mac session (a real
screen-share sharer that never throttles) avoids this entirely.

**Iterating on the native code during a session (edit → rebuild → re-observe):**
- **Pre-build the binary before `dev:clean`.** Run
  `cd apps/desktop/src-tauri && cargo build --bin desktop` FIRST, then launch
  `npm run dev:clean`. `tauri dev` otherwise does a two-step launch — it starts
  the app from the *stale* existing binary, then rebuilds but often does NOT
  relaunch into the new binary — so you end up observing the old code. Building
  first makes the binary current before launch. (Confirm: the running
  `target/debug/desktop` mtime should be ≥ your last source edit, and its process
  start time ≥ that mtime.)
- **Every native relaunch drops the browser peer** back to the room list — after
  each `dev:clean`, rejoin in the browser (click **Join**) and re-share (**Share
  test pattern**; if it already shows **Stop test pattern**, click it once to
  stop, then Share again to force a fresh publish). A stop→start of the share is
  also how you force the native side to rebuild a window fresh (vs. reusing a
  retired one from its warm pool, which skips the resize/reposition path).
- **Re-export the `PETAL_AUTOTEST_*` env on every relaunch** — it is NOT
  inherited if `tauri dev` restarts the child on its own; always relaunch
  `dev:clean` with the env block explicitly.
- **Trace child-window geometry with temporary DIAG logs.** Adding a
  `log::info!` in `reposition_chrome` (requested vs. actual `outer_position`
  read-back) and at the top of the panel's `WindowEvent` handler is how the
  `(0,0)`/`(0,43)` corruption was pinned down — it proved a child's
  `set_position` does **not** stick when called synchronously inside the panel's
  own Moved/Resized handler (AppKit reasserts the child follow-offset as the
  handler unwinds), which is why the overlay sync is deferred to the next
  main-thread turn.

## Manual Cross-Client Test (desktop + browser, real permissions)

A repeatable manual procedure for validating cross-client camera, window
sharing, and remote control against the real installed release app plus a
real browser client — the gap the automated tiers above don't cover, since
they don't exercise two independently-permissioned clients talking to each
other live. Used to validate #109/#110 and to find #116/#117.

**Why not the chrome-devtools MCP browser for the second participant:**
whatever browser that MCP drives has no camera/screen-recording OS
permission, and its native permission prompts aren't reachable — they don't
appear in this app's window (filtered from computer-use screenshots), aren't
in the accessibility tree of either Chrome process System Events can see
(it's a separate, unlisted process), and computer-use only grants browsers
read-only access (no click/type). Requesting the prompt be granted via
AppleScript UI-scripting on "Google Chrome" is a dead end too if the user has
more than one Chrome process running — `System Events` resolves `process
"Google Chrome"` ambiguously and you can end up clicking the wrong window
entirely. **Use the `claude-in-chrome` MCP against one of the user's own,
already-permissioned Chrome profiles instead** (`list_connected_browsers` /
`select_browser` if more than one is connected — ask the user which one).
This gets you a real camera/mic/screen-recording-capable browser tab with no
permission dance at all.

### Setup

1. Confirm the desktop release app (`/Applications/Petal.app`) is running and
   already joined a room (or join one: paste a room name into the main menu
   and click Create, or click an existing room in "YOUR ROOMS").
2. In the desktop app, click **Copy invite link** and read the clipboard
   (`pbpaste`) to get the real URL, e.g.
   `https://meet.petal.live/<room-name>/<access-code>`.
3. Via `claude-in-chrome`: `tabs_context_mcp({createIfEmpty: true})`, then
   `navigate` to `http://localhost:5184/` (web-harness dev server) in that tab.
4. Fill the name field, then fill the meeting field with the **full invite
   URL** and **wait ~1s before clicking** — the button label flips from
   "Create" to "Join" once the paste is recognized as an invite link. If you
   click while it still says "Create", you'll create a brand-new room named
   after the literal pasted URL string instead of joining the existing one
   (happened once this session — that stray room can't be deleted from the
   UI, so don't repeat the mistake; always screenshot/confirm the button says
   "Join" first).

### Camera (bidirectional)

1. Click **Video** in the browser tab's toolbar. In a real permissioned
   profile this just works (no prompt). Confirm with a screenshot that your
   own tile shows real video, not a flat placeholder.
2. On desktop, drive the meeting window via AppleScript/System Events (the
   app's window and controls aren't visible to computer-use, but they are
   fully driveable via `osascript`/System Events since Tauri's webview
   content exposes normal `AXCheckBox`/`AXButton` elements):
   ```
   osascript -e 'tell application "System Events" to tell process "desktop"
     set allEls to entire contents of window "Petal"
     repeat with e in allEls
       if (title of e) is "Turn camera on" then click e
     end repeat
   end tell'
   ```
   (Process name is `desktop` — Tauri's crate name — even for the signed,
   installed release bundle, not "Petal".)
3. **Gotcha:** if desktop's camera preview times out ("Camera request timed
   out — it may be held by another app"), something else (often the browser
   tab from step above, or Zoom/FaceTime) already holds the physical camera —
   turn that off first, then retry. Not a bug; only one process can hold a
   physical camera at a time.
4. Confirm each side sees the other's live video in its tile (not just its
   own self-view) via a screenshot on both ends.

### Window / screen sharing

1. On desktop, click the **Share a window** checkbox (`AXCheckBox`) to enter
   share-picking mode, then move the real mouse (`cliclick m:X,Y` in small
   steps — a single warp doesn't reliably fire the hover tracking) over the
   target window's top edge until a `Hover Tab` window appears
   (`osascript -e 'tell application "System Events" to get name of every
   window of process "desktop"'` should list `Hover Tab`), then click its
   **Share this window** button the same way as the camera checkbox above.
2. **Use a fresh, blank TextEdit document as the share target for any test
   that also exercises remote control** (`osascript -e 'tell application
   "TextEdit" to make new document'`) rather than a real window with your
   actual work in it — remote control will be typing into it.
3. Confirm the shared window's content renders in the browser tile.
4. **Known gotcha (#117):** if the shared window's content never visually
   changes, a 45s watchdog (`session/share.rs`) restarts the share as a
   precaution — expected — but this currently also silently drops any active
   remote-control session (see below). Don't be surprised if "Controlling"
   reverts to "Request control" with no explanation around the 45s mark.

### Remote control

1. In the browser tab, click **Request control** on the shared-window tile.
   With remote control left enabled (the default — desktop's "Disable remote
   control" checkbox describes the *action*, so seeing that label means it's
   currently *allowed*), control is granted immediately, no approval prompt.
2. Click once inside the video tile (establishes pointer target + focus),
   then type. Check `~/Library/Logs/Petal/petal.log` for
   `remote-control: received kind=Pointer`/`kind=Key` and
   `remote-control-latency: host replay complete` lines to confirm delivery
   at the transport layer.
3. **Verify it actually landed**, don't just trust the log — the transport
   can report success while the input lands on the wrong element (#116).
   Read the real target app's content directly, e.g. for TextEdit:
   ```
   osascript -e 'tell application "System Events" to tell process "TextEdit" to return value of text area 1 of scroll area 1 of window 1'
   ```
4. **Known gotcha (#116):** if replay logs show
   `AX pointer down ... role=AXGroup attempted=pressable,text_selectable
   failed`, the click hit a non-interactive element — the window's tracked
   coordinate frame doesn't match its real on-screen bounds (more likely
   right after a share-border panel got reused from a previously-shared,
   differently-sized window). Re-sharing the window fresh (stop, then share
   again) may realign the frame; if not, this is the open bug, not a repro
   mistake.
5. To test revoke: toggle desktop's **Disable remote control** checkbox off,
   then confirm the browser side can no longer inject input (clicks/keys stop
   producing `remote-control: received` log lines).

#### Observability (#372)

Two host-side additions make replay health visible without hand-parsing every
per-event `remote-control-latency:` line:

- **Failure nack.** A replay failure (`input::replay` returning `Err`, e.g.
  Accessibility revoked mid-stream or an AX/injection hard-fail) now sends a
  status packet back to the controller instead of only `log::warn!`-ing —
  `accessibilityDenied` for AX-revoked, `targetUnavailable` for everything
  else. Throttled to at most one such status per second per (window,
  controller) so a sustained failure stream doesn't spam the controller.
  Reproduce: grant control, then flip System Settings > Privacy & Security >
  Accessibility off for Petal mid-hold; the controller's tile should switch to
  the existing "Needs access" chip within ~1s (`remoteControlFeedback.ts`),
  not silently keep accepting dead input.
- **Periodic summary.** Every ~30s (piggybacked on the existing 250ms
  held-input TTL-sweeper tick — no new thread), the host logs one
  `remote-control-summary:` line to `petal.log` with injection `elapsed_ms`
  p50/p95/max and success/failure counts since the last tick, plus the
  running `replay_high_rate_drops`/`resolve_high_rate_drops` counters. Useful
  for turning a "control felt laggy" report into a number without grepping
  hundreds of individual `remote-control-latency:` lines.

### Cleanup

Leave the meeting on both ends when done. There's no way to delete a room
from the UI (#87, closed but the delete gap itself wasn't back-filled) — any
room created by an accidental "Create" click (see Setup step 4) will persist
in "YOUR ROOMS" forever; just avoid creating new stray ones rather than
trying to clean up existing ones.

## Desktop Autotest Scenarios

The desktop app has an env-gated debug command socket in `apps/desktop/src-tauri/src/autotest.rs`. It is off unless `PETAL_AUTOTEST_ROOM` or `PETAL_AUTOTEST_SOCK` is set.

The JSON scenario runner is `apps/desktop/scripts/autotest-run.mjs`:

```sh
cd apps/desktop
node scripts/autotest-run.mjs scripts/scenarios/s35-smoke.json "$PETAL_AUTOTEST_SOCK"
```

Scenario files live in `apps/desktop/scripts/scenarios/` and have this shape:

```json
{
  "name": "s35-smoke",
  "commands": [
    { "name": "state", "cmd": "dump_state", "expect": { "ok": true } },
    { "sleepMs": 250 },
    { "name": "accessibility", "cmd": "accessibility_status", "expect": { "ok": true } }
  ]
}
```

Each command object is sent as newline-delimited JSON to the Unix socket after removing runner-only fields: `name`, `expect`, `expectOk`, and `sleepMs`. `expect` checks response fields by dotted path, and `expectOk: false` asserts that a command fails.

For #298's local reconnect proof, a QA build (`--features cockpit-privileged`) or
autotest build may send exactly one `{"cmd":"reconnect","mode":"resume"}` or
`{"cmd":"reconnect","mode":"full"}` after joining. These map directly to the
LiveKit SDK's resume and full-reconnect simulations; they do not close/rejoin the
room or alter local shares. A test process accepts exactly one reconnect request,
including a failed SDK request; restart it for a second attempt. The command is
unavailable from a normal release build and the owner-only socket remains inert
unless `PETAL_AUTOTEST_SOCK` is set.

Checked-in scenarios:

- `s12-pill-drag-preflight.json` - accessibility and state preflight.
- `s22-hover-tab-input-preflight.json` - accessibility and hover-tab state preflight.
- `s35-smoke.json` - state and accessibility smoke test.

Issue #680's panel-prewarm stress harness is deliberately opt-in because it
needs a running macOS app, a joined room, Screen Recording permission, and two
real on-screen sacrificial windows. It sends only the existing `share`,
`stop_share`, and `dump_state` commands, so every transition enters the real
`hover_tab::toggle_share_for_window` path. For each iteration it starts the
first window, starts the second while the first remains live, and requires one
main-thread focus-handback liveness marker for each share generation before
unsharing both. Current builds use #677's unconditional `measure summary`
marker; the harness also recognizes the older `selection handback observation`
name cited in #680.

With a debug/autotest Petal process already joined and listening on the socket,
list safe candidate window IDs and then run 25 two-window iterations:

```sh
cd apps/desktop
node scripts/issue680-panel-prewarm-stress.mjs \
  --socket "$PETAL_AUTOTEST_SOCK" --list-windows
node scripts/issue680-panel-prewarm-stress.mjs \
  --socket "$PETAL_AUTOTEST_SOCK" \
  --window-id <first-sacrificial-window-id> \
  --window-id <second-sacrificial-window-id> \
  --iterations 25
```

This display-requiring stress run is not part of `scripts/ci-local.sh`; the
script exits nonzero on a socket timeout, state mismatch, duplicate generation,
or missing liveness marker.

`apps/desktop/scripts/remote-control-harness-preflight.mjs` is the headless remote-control
preflight. It validates the shared remote-control contract fixture, checks that
the expected native/web harness vectors are still present, then runs
`cargo test --lib remote_control --locked` and `web-harness`'s `npm test`.
Pass `--check-only` to do only the static fixture/inventory proof plus the
required macOS Swift/AppKit sentinel typecheck. The only exception is the
explicit Ubuntu portability command `--check-only --skip-swift-typecheck`:
it still runs every portable assertion and reports the sentinel as `DEFERRED`
to the named macOS CI gate. Normal local/macOS preflight remains fail-closed.

`apps/desktop/scripts/remote-control-local-loopback.mjs` packages the remote-control
local-loopback harness. Its `--check-only` mode runs only CI-safe inventory
checks. Its `--live` mode prints the required setup, then drives the live
scenario against a sacrificial TextEdit document and enforces acquisition,
native host status, and named target-observation latency thresholds. Add
`--press-to-photon` to replace the TextEdit matrix with a local external AppKit
sentinel and gate web-input-to-estimated-browser-display p95 across real text
and left-click reactions:

```sh
scripts/rc-live-suite.sh --press-to-photon
```

That command brings up the local LiveKit server, web harness, Chrome debug
session, Petal dev app, and sentinel. When those services are already running,
the lower-level runner is:

```sh
PETAL_AUTOTEST_SOCK=/tmp/petal-rc.sock \
  node apps/desktop/scripts/remote-control-local-loopback.mjs \
  --live --press-to-photon --json /tmp/rc-photon.json
```

Add `--input-only` instead to run the SAME 30-case matrix with a
video-independent share-readiness bar (plan 6c). It keeps the identical CDP
probe and changes only the acceptance predicate: a share counts as ready once
the publication is present as a controllable target, rather than once a decoded,
sized video frame is on screen. `assertShareBorderStacked` stays fatal in both
modes -- it is a WindowServer readback, not a video read.

```sh
scripts/rc-live-suite.sh --input-only
```

**It is not "runs with capture dead," and it does not rescue every observed
failure.** It relaxes the share-readiness predicate only; `start_share` still
blocks on a first captured frame. Two distinct failure shapes appear in the
2026-08-10 logs:

- **(a) `share` returns and the tile never goes live.** `--input-only` rescues
  this. `petal-dev-rc3.log` is this shape: `session::share` logs 230 lines, and
  the snapshot pull ran and pulled 205 snapshots without a single failure.
- **(b) `start_share` itself never returns.** `--input-only` does NOT rescue
  this and will hang in the same place, because the relaxed predicate sits
  downstream of `start_share`. `petal-e2e-final.log` and `petal-dev-781.log` are
  this shape: `session::share` logs exactly four lines and stops at
  `starting SCStream capture via direct-window-id (attempt 1/3)`.

The common root cause of both is that the capture source window is not drawing --
every empty sample logs `SCK sample with NO image buffer status=Some(Idle)
dirty_rects=0 (stream alive, source not drawing)`, 1932 times in
`petal-e2e-final.log`. The snapshot-pull rescue cannot help shape (b) at all: the
pull lives inside the share pump, which only starts after `start_share` returns.

What it does prove, in shape (a): the controller -> data channel -> host
authorization/grant -> CGEvent/AX replay -> real NSEvent receipt chain in a
foreign AppKit process. It proves nothing about pixels reaching a viewer,
press-to-photon latency, or encode/decode.

Three things stop a relaxed run being cited as the full gate: the SUMMARY
carries `mode` and `shareReadiness`; the runner and wrapper both print
`INPUT-ONLY -- video path NOT verified`; and results go to a distinct artifact,
`/tmp/rc-results-input-only.json`, which is `rm -f`'d first (an explicit
`--json` path whose filename does not contain `input-only` is refused).
`--input-only` cannot be combined with `--press-to-photon` or
`--rapid-click-burst`, the only two modes that read video frames.

Its pass bar is fixed in code as `INPUT_ONLY_PASS_BAR_CASE_IDS`
(`apps/desktop/scripts/remote-control-share-readiness.mjs`) so it cannot be
quietly lowered: cases **5, 8, 15, 16, 21, 25, 26, 28, 29, 30** -- exactly the
cases whose oracle is the sentinel's own foreign-process NSEvent ledger -- must
all report `pass`, and an unmet bar forces exit 1 even when nothing FAILED (a
skipped bar case contributes no failure count). Case 23
(Retina/secondary-display mapping) is excluded by name because it needs a second
display: it may skip, it may never fail.

This measurement includes LiveKit control delivery, host replay, native target
reaction, ScreenCaptureKit capture, encode/publish, web decode, and browser
frame presentation scheduling. It uses `requestVideoFrameCallback`'s
`expectedDisplayTime`, so it is a software photon-time estimate rather than a
physical-panel photodiode measurement.

`.github/workflows/nightly-loopback.yml` runs that `--live` path on a nightly
schedule plus `workflow_dispatch`. It intentionally targets only
`runs-on: [self-hosted, macOS]`: the repo owner must register a real logged-in
Mac runner with that label, Chrome installed, `livekit-server` on `PATH`, and
Screen Recording/Accessibility grants already approved for the launched Petal
dev binary/runner session. A GitHub-hosted macOS runner cannot satisfy those
TCC or display-session requirements.

The nightly catches regressions that `--check-only` cannot: web-controller
messages crossing LiveKit, native host activation/status, and real replay into
TextEdit through the live macOS input path. A failure means either the artifact
scorecard contains one or more `fail` results, the native app never joined or
opened the autotest socket, or the self-hosted runner lost one of its live
prerequisites. Inspect the uploaded `nightly-loopback-*` artifact first:
`remote-control-live.log` has the parsed RESULT/SUMMARY lines, while
`petal-dev.log`, `web-harness.log`, `livekit.log`, and `chrome-cdp.log` separate
app bugs from runner setup/TCC/display problems.

`apps/desktop/scripts/remote-control-scenario.mjs` is the live scenario used by that
wrapper. It uses the same autotest socket plus Chrome DevTools and TextEdit; it
is not driven by a scenario JSON file.

### Which RC test do I run?

| Scenario | Which tool | Why |
|---|---|---|
| Comprehensive, human-run, or nightly validation | `scripts/rc-live-suite.sh` | Runs the full 30-case remote-control matrix and is the canonical live suite for broad regression coverage. |
| Fast, Test Cockpit-embedded, or prod-realistic smoke check | `node apps/desktop/scripts/cockpit.mjs --test-case=RC-P1080` | Runs the intentionally narrow RC-P1080 smoke check through the Test Cockpit's in-process Rust engine against prod LiveKit; it is not comprehensive coverage or a replacement for the 30-case suite. |
| Latency-specific validation | `scripts/rc-live-suite.sh --press-to-photon` | Replaces the matrix with the press-to-photon gate, measuring web input through native reaction to the estimated browser display. |
| Second-Mac validation | `scripts/cross-machine-rc-suite.sh` | Runs the remote-control suite with the sharer on a genuinely separate, SSH-reachable Mac, exercising the cross-machine path. |
| Petal as the CONTROLLER (native→native, native→web) | `--test-case=RC-N2N` / `--test-case=RC-N2W` (see below) | Everything above drives a WEB controller against a native host. RC-N2N is the only harness that drives the real native controller route, which is the direction two Petal users at two Macs actually use. |

#### RC-N2N / RC-N2W — Petal as the controller (#819)

Journey RC-07. One command each, after the one-time setup below:

```bash
cd apps/desktop
scripts/build-cockpit-primary.sh        # the primary (controller) binary
scripts/build-test-peer.sh              # the test-peer (host) binary
scripts/cockpit-setup.sh                # one-time TCC grants + setup marker
RUST_LOG=info ./src-tauri/target/debug/desktop --test-case=RC-N2N > /tmp/rc-n2n.log 2>&1 &
```

RC-N2N needs the test-peer to hold **Accessibility** as well as Screen
Recording — it is the host, so it is the process that injects. Without it the
scenario reports INFRA-FAIL naming the binary to grant, never a product
failure: an un-trusted host accepts a grant and then injects nothing, which
would otherwise read as "remote control is broken".

The run opens its own sacrificial TextEdit document (title marker
`petal-rc-n2n-<runid>`) and closes it again on every exit path. The marker is
unique per run, so a document left behind by a killed run cannot be picked up
by the next one — but it does stay open, and the windows accumulate. Sweep them
occasionally. The keystone set includes normalized native Copy and Paste, and
host-side clipboard operations can clobber the machine's system clipboard.
**A run changes the machine's clipboard.** Don't schedule one while you're
holding something you meant to paste. This same-machine scenario proves native
routing/actuation only; it cannot prove transfer between separate A/B
clipboards because both local instances share one OS clipboard. The run also
briefly activates TextEdit several times (at document open and before each Cmd chord):
on a single Mac the peer instance's own startup re-activates itself and
backgrounds TextEdit, and a backgrounded app's `AXFocusedUIElement` does not
resolve — which silently downgrades the host's AX select-all/copy to a
CGEvent key-equivalent that cannot act on a non-key window. Also required:
the console must be UNLOCKED (the scenario preflights this and says so). Separately, the scenario refuses to start against a document that
already contains its keystone text, so "the typed text landed" can never be
satisfied by content that was there before the drive.

`RC-N2W` is the same controller against a browser peer. It proves DELIVERY
only — the harness records what it receives and never claims an input was
applied, because a browser cannot inject OS input. Its keystone deliberately
excludes the native-only clipboard commands; a browser peer ignores the native
Copy request and clipboard byte-stream topic.

#### Native clipboard checks

Native control keyboard shortcuts have fixed boundary meanings:

- `Cmd+C`/`Ctrl+C`: Copy from the sharer's shared window to the controller's
  clipboard (B→A).
- `Cmd+V`/`Ctrl+V`: send the controller's plain text to the sharer's clipboard
  and then paste into the shared window (A→B).

The automated native tests cover validation and host actuation. To prove the
actual transfer, use separate OS clipboard sessions or two machines and seed
A and B with different values. Verify remote Copy changes A, verify a local A
clipboard change during Copy causes the delayed response to be discarded, and
verify remote Paste leaves B's clipboard updated even when target invocation
fails. A second Petal process on one desktop shares the same clipboard and is
not valid transfer evidence.

These shortcuts do not support Copy/Paste solely within B. For that use case,
use the target application's context menu, toolbar, or dropdown for both Copy
and Paste, if that UI is reachable inside the shared window. Do not combine an
application-menu Copy with Petal's keyboard Paste, because Petal keyboard
Paste intentionally replaces B's clipboard from A. If no suitable in-window
UI is reachable, the B-local use case is unsupported. Rich formats, files,
file lists/file promises, empty text, NUL-containing text, and text over 1 MiB
are rejected; path-looking ordinary text is allowed.

Both are on the opt-in `gap` tier, so `quick`/`full`/`soak` sweeps do not
depend on the extra grant.

**Live status (2026-07-05, STALE — see 2026-07-07 below): the 28-case suite
was green** — 19 pass / 0 fail / 9 skip (all skips are explicit "needs a
sentinel app"/secondary-display/extra-socket-command gaps, not failures), run
via `scripts/rc-live-suite.sh` from the repo root. Getting here required
replacing `CGEventPostToPid`-based pointer/drag/scroll replay with
Accessibility-API-based direct manipulation (`AXPress`,
`AXSelectedTextRange`+`AXRangeForPosition` for drag-to-select, scrollbar
`AXValue` for scroll, `AXShowMenu` for right-click, gated to skip on an actual
drag so it can't orphan a context menu) plus a private `SLEventPostToPid`
click supplement — `CGEventPostToPid` alone never had a real effect on
pointer/drag/scroll for this target, only keyboard. See `remote_control.rs`'s
`mod input` for the implementation and `internal/archive/PROGRESS.md`'s 2026-07-05
entry for the full debugging trail.

**Live status (2026-07-07): 12 pass / 5 fail / 11 skip, reproducible across 3
clean back-to-back runs.** The 5 new failures (cases 6, 10, 11, 12, 13) all
cluster around establishing/reading a text selection (drag-select, Cmd+A
select-all, and every clipboard op that depends on a prior selection) — every
other case still passes. `git log` shows zero commits to `remote_control.rs`
or `remote-control-scenario.mjs` since the 2026-07-05 baseline, so this is an
environmental regression, not a code change in this repo. Filed in the project tracker with full repro evidence
and a root-cause hypothesis; not yet fixed. Separately, this same 2026-07-07
sweep found and fixed the harness's own join mechanism, which had been
silently broken since the #104 access-code redesign landed the same day as
the 2026-07-05 baseline run — meaning that baseline run's "19/0/9" was
recorded just before #104 shipped, and nobody re-ran the live suite between
then and now to notice the join path had broken. Lesson: a documented "live
status" line for a live/human-only suite goes stale silently — nothing
re-runs it automatically. Re-run `scripts/rc-live-suite.sh` after any change
near `meetingCode.ts`/`rooms.rs`/`remote_control.rs` and update this line.

The numbered remote-control matrix is now 30 cases. Cases 5, 8, 15, 16, 21,
25, 26, and 28 use the standalone AppKit sentinel's JSONL event log plus the
owner-gated `remote-control-status`/`remote-control-disable` socket commands;
case 23 runs automatically when `displayplacer list` reports a second display.
Case 29 exercises a resume/full reconnect while control and a press are held.
Case 30 sends one safe left click to the AppKit sentinel, waits for its terminal
result, and republishes that completed v2 operation through a test-only no-arg
hook with a fresh outer transport sequence. It passes only when two strictly
correlated, identical terminal dispositions are observed in order while the
sentinel records exactly one native accessibility action. Both terminal
records may consistently omit the optional delivery route/failure code for
compatibility with peers that do not publish that metadata. The hook audits
through the original advertised dedup expiry; any conflicting or extra
same-operation terminal taints the proof and fails the case.

`scripts/cross-machine-rc-suite.sh` runs this same suite with the sharer role
on a genuinely separate, SSH-reachable second Mac instead of one Mac talking
to itself. It accepts only physical `arm64` and `x86_64` peers, with all four
release-evidence pairings required: arm64→arm64, arm64→x86_64,
x86_64→arm64, and x86_64→x86_64. It rejects Rosetta/translated execution
before any remote bundle mutation, so an `x86_64` process alone is never
Intel-hardware evidence.

The harness builds one explicit universal, Developer-ID-signed **QA/autotest**
bundle on the controller, verifies its slices, bundle ID, team, hardened
signature, and hash before and after transfer, then launches the exact same
bundle remotely. This diagnostic bundle is not a customer release binary and
does not itself certify packaged production-default behavior. The three
direct-route launchd toggles are cleared before launch so the result exercises
the packaged-default route.

Before deployment, SSH key access, a remote Aqua/console session, and a
minimal AppleEvent preflight are mandatory. Screen Recording and Accessibility
must be manually granted to the stable signed QA bundle; the harness never
resets or changes TCC permissions. A missing grant or autotest socket is a
classified failed run. The only retained evidence is an allowlisted summary
(major macOS version, architecture, signing team/fingerprint/hash, terminal
counts, and the bounded case-30 recovery verdict/count/terminal records); raw
shell/app/scenario logs, credentials, identities, window titles, paths, typed
text, coordinates, OS errors, screenshots, and media are never collected.
The reducer forces a failed suite result if that recovery proof is missing or
contradictory. Safe evidence is retained by default in a mode-0700 run directory
under `${TMPDIR:-/tmp}/petal-cross-machine-evidence` (override the parent with
`PETAL_CROSS_MACHINE_EVIDENCE_DIR`); private raw JSON/stdout is removed on every
exit. Not yet live-validated against a real second Mac (tracked in issue
#79; Intel blocker implementation #539).

**Live status (2026-07-14): 18 pass / 10 fail / 1 skip**, up from a
regressed 10/19/0 baseline recorded the same day. That regressed run's
19 failures were triaged (not all real): 2 were the confirmed middle-click/
drag opt-in-gating bug (#446 — `direct_drag_enabled()`/`direct_scroll_enabled()`
default off, still true today and still failing cases 5/8/21 as expected),
and 13 of the remaining 17 were a single TextEdit-wedge cascade (cases
19-29 all failing identically) plus stale/weak harness assertions (case 4's
unsatisfiable predicate, case 7's wire-echo-only check) — fixed in #455.
After the fix: case 4 passes against the real wire shape, case 7 now
verifies the real host-side effect (`remote-control-status`'s
`pressedInputs`, not the controller's own wire echo — the prior version
would have silently passed even if the drag's Up never actually released),
the wedge cascade is gone (cases 19-29 now succeed/fail independently on
their own merits), and one more harness bug (case 26's illegal top-level
`return`, previously invisible under the cascade) was found and fixed
during this same verification pass. Remaining 10 failures: cases 5/8/21
(#446, expected), cases 15/16 (Ctrl/Alt — native-side replay completes
cleanly per petal.log, but the harness's own `setTextEditDocument` calls
AppleScript `activate` before every case including sentinel-targeted ones,
stealing focus right before dispatch — a harness bug, not yet filed), and
cases 22/25/28/29 (each got a genuine independent read for the first time
now that the cascade is gone — worth a fresh investigation pass, not yet
triaged). Also found: `scripts/rc-live-suite.sh`'s own wrapper hits a
macOS-bash-3.2 `set -u` "unbound variable" bug on `"${MODE_ARGS[@]}"` when
empty (i.e. every default, non-`--press-to-photon` run) — worked around by
invoking `remote-control-local-loopback.mjs --live` directly; not yet fixed
in the wrapper itself.

**Live status (2026-08-14, CURRENT): 27 pass / 2 fail / 1 skip**, up from
**2 pass / 28 fail** at the start of the same session. Full video path
(`shareReadiness: live-tile`), `recoveries: 0`, `tokenlessDrops: 0`,
target-observation latency p95 221ms against a 500ms budget. Five defects
closed the gap, in this order — each one had to land before the next was even
visible:

| # | defect | effect on the suite |
|---|---|---|
| #804 | two authorities wrote the capture output size and fought over it | every share tore itself down; cases 3-30 failed against a DEAD share, not a looping one |
| #806 | a static source tripped the 45s wedge watchdog | the second share died ~2m15s in |
| #807 | the recovery circuit breaker counted LIFETIME restarts | what turned both of the above into a dead share rather than a hiccup |
| #808 | a `stopped` cleared the web controller's session and no later `active` could re-establish it | cases 16, 26, 29 (and initially 30) reported `granted:false, grantToken:null` |
| — | two stale scenario predicates (cases 5, 22) | both waited for a pointer `action: 'down'` publish metric that `api.click()` has not sent in a long time, so both died on a 7s timeout BEFORE reaching their real oracle |

The 2026-07-14 entry above is superseded: the `MODE_ARGS[@]` bash-3.2 bug is
fixed (the wrapper runs clean), and the cases it lists as failing for
focus-stealing reasons now pass.

**Update (2026-08-15, five further runs):** case 21 is FIXED (#811 closed —
the case's NSEvent oracle was structurally unsatisfiable; it now asserts the
sentinel's scroll POSITION via a new horizontally scrollable strip, matching
how scroll actually replays). Best runs remain **27 / 2 / 1**. The only
CONSISTENT failures are **29/30 = #820's residue**: after a host resume,
arriving input is refused `reason=auth detail=no-active-request` even though
the stale-disconnect revoke is now averted (the grace-confirmed revoke landed
`e0cf46bc` and measurably keeps grants through 2 stale aftershocks per run),
and a replayed op's terminal result does not reach the controller. Cases
7/10/14 each flaked exactly once across five runs under heavy parallel load
and pass otherwise. Case 23 skips without a second display, by design.

**Exit-code caveat for anyone automating "is the suite green":** the suite
exits non-zero on ANY failed case, including the known #820 residue — there is
no expected-failures allowlist. Until 29/30 are fixed, a supervising script
must parse `/tmp/rc-results.json` and compare the failing case IDs against the
known set rather than trusting the exit code; treat any failure OUTSIDE
{29, 30} (or a skip of a pass-bar case) as a real regression.

**Do not read the #446 note in the entry above as still explaining cases 5/8/21.**
Case 5 and case 8 pass. The three direct routes
(`PETAL_REMOTE_CONTROL_DIRECT_CLICK/DRAG/SCROLL`) do still default off, and
that is deliberate — `scripts/cross-machine-rc-suite.sh` clears them so a run
exercises the packaged default — but only case 21 is still blocked by a
missing route, and horizontally only.

### What a green run of this suite does NOT prove

Write this down next to any "the suite is green" claim, because two of these
were nearly cited as evidence in the session that produced the numbers above:

- **The capture ROI abandonment fallback never executes.** Healthy SCK
  acknowledges every ROI on the first attempt, so #804's `abandon_roi`, its
  attempt budget, and its "keep the padded raster and stay published" behaviour
  run only in unit tests.
- **The static-source path is never reached.** The suite keeps its target
  window active; the longest raw silence in a full run was **8.6s**, and the
  wedge watchdog does not arm until 45s. Zero wedge restarts in a suite run is
  *consistent with* #806's fix and is not evidence of it. See "Reproducing a
  genuinely static source" below for the run that is.
- **Nothing timing-dependent.** The #804 fps/ROI race was microseconds wide; a
  green run says nothing about it either way.
- **Nothing about a second display** (case 23) or a second machine.

This human-run suite's status log is separate from the Test Cockpit's automatic
baseline-diffing system. After a Cockpit run, use
`scripts/cockpit-baseline-compare.mjs` as documented below to detect per-machine
regressions; the comparison does not update or replace the live-status entry
above.

## Test Cockpit

### Evidence basis and baselines (#457)

Every scenario verdict carries an `evidenceBasis`: `HostEffect` means the
oracle observed the native host effect, while `ContentVerified` means decoded
content was checked. `WireShape`, `LivenessProxy`, and `Scaffold` are weaker
signals. A passing weaker signal is rendered mechanically as `PASS (proxy — not
content-verified)` in the run conclusion. `RC-P1080` is tagged `HostEffect`
because its intended oracle compares the host sentinel ledger; live execution
still depends on the active-share setup tracked by #470.

Each real cockpit run now compares its artifacts with the per-machine,
ignored `baseline.json` automatically. The comparison is embedded in the
conclusion and also written to `baseline-comparison.json`; it reports baseline
age, Petal/macOS environment drift, pass-to-fail changes, evidence-basis
degradation, and p95 latency increases above 20%. Only an exact `quick` or
`full` tier run whose scenarios all pass with `HostEffect` or `ContentVerified`
may update the baseline; narrow scenario runs never replace it. Baseline
regressions are diagnostic and do not change the run's own pass/fail status.
A missing `scorecard.json` or malformed `run.jsonl` is surfaced by the viewer
as `incomplete`; the conclusion states `run aborted before verdict` when an
aborted artifact is inspected.

This baseline-diffing system is separate from the human-run 30-case RC suite's
Live status log above. Use the live-status entry for the latest manual
`scripts/rc-live-suite.sh` result, and use this comparison for per-machine Test
Cockpit baseline regressions; neither record replaces the other.

The Test Cockpit is a self-driving validation harness (see GitHub issues
#253-#265) that validates Petal's core features/perf against **prod**
LiveKit, triggerable from a Settings-UI button or launch params. It drives real
prod scenarios end-to-end (native + a headless web peer) and scores them
PASS/TEST-FAIL/INFRA-FAIL. It is separate from the tiers above, which stay
CI-safe and local-livekit where applicable. This section documents the cockpit
setup (#253), the SHARE-W2N-Q walking skeleton (#254), and the #257 cockpit
engine: scenario table, step executor, Tauri commands, result artifacts, and
the `cockpit.mjs` wrapper. It also covers #265's saved-run results viewer and
manual-verification artifacts. Some Full/Soak chaos scenarios still require
live machine provisioning before they can do destructive mutations; those
scenarios must skip or infra-fail honestly until that setup exists.

**Journey-table honesty (#379 step 1):** the journey table
(`internal/docs/COCKPIT_TEST_MAP.md`) marks `RC-01`..`RC-06` (remote control) and
`RES-04` (display sleep) as **⛔ Gap**, not ✅ Covered. `RC-P1080` now drives
representative input through LiveKit and compares it with the sentinel's
host-side NSEvent ledger in both directions; wire echo is diagnostic only. Its
scorecard carries host-event latency p50/p95/max statistics. The live execution
and deliberate middle-click red-run proof remain pending orchestrator
verification. `RES-04` has no runnable scenario at all; automatic baseline
diffing remains tracked separately under #379.

### Cockpit status

RC-P1080 is an intentionally narrow smoke check, not a replacement for the
comprehensive 30-case suite. Its cockpit driver sends click, middle-click, drag, typing, shortcut,
and scroll gestures through the existing LiveKit transport. Rust verifies the
sentinel JSONL NSEvent ledger against the driven ledger in both directions;
controller wire echo is diagnostic only. The scorecard records host-event
latency p50/p95/max. A live execution and the deliberate middle-click red-run
proof remain required operator verification.

Update this bounded status block after a QA cockpit run with:

```sh
node apps/desktop/scripts/update-testing-status.mjs [results-dir]
```

With no `results-dir`, the script uses the latest
`~/Library/Logs/Petal/test-runs/<timestamp>/` directory (by the run timestamp
in the directory name, not mtime). It rewrites only the marked block below from
`run.jsonl` when available, or `scorecard.json` when the run log is absent.

`passed` requires positive evidence (#622): at least one verdict, zero
failures, zero unrecognised verdict strings, and a complete run (`conclusion`
event present with matching scenario counts; every `scenario-start` reaching a
verdict). A crashed partial run, an unrecognised verdict (`error`, `timeout`,
`crashed`, ...), or an `--expect-total N` mismatch publishes as failed; zero
verdicts publishes as `INSUFFICIENT DATA`. The script exits nonzero whenever
the published status is not `passed`.

<!-- cockpit-status:start -->
Last updated: 2026-07-26T16:42:19.443Z

| Field | Value |
|---|---|
| Results dir | `~/Library/Logs/Petal/test-runs/1785084128968` |
| Artifact | `run.jsonl` |
| Run ID | `1785084128968` |
| Tier | `share-n2n` |
| Status | `failed` |
| Passed | 0 |
| Failed | 1 |
| Skipped | 0 |
| Unrecognised | 0 |
<!-- cockpit-status:end -->

### One-time setup

Run once per test machine, by a human, before any cockpit work:

```sh
scripts/cockpit-setup.sh
```

It builds the test-peer binary (`target-peer/debug/desktop`, a wholly separate
binary from `target/debug/desktop` used by the native-native `SHARE-N2N`
scenario, #262 -- built, live-verified, and passing since commit `1b638df3`),
walks you through granting Screen Recording + Accessibility
for **both** binaries, triggers the one-time Automation/AppleEvent consent
`osascript` needs to control TextEdit (dev-tier remote-control readback only),
and prints (does not install) the sudoers snippet `scripts/net-impair.sh`
(#261's CHAOS-NET stub today) will eventually need. Installing that sudoers
entry is a system-level security change the script deliberately leaves to a
human running `sudo visudo -f /etc/sudoers.d/petal-net-impair` themselves --
see the script's own printed instructions for the exact snippet. The script is
idempotent (safe to re-run; already-granted steps are skipped) and writes a
local marker file (`~/Library/Application Support/com.petal.app/.cockpit-setup-complete`)
only once every grant it can automate is confirmed via non-prompting checks.

### The preflight-and-refuse contract

This is the mandatory rule every later Test Cockpit engine entry point
(`start_test_cockpit` and any privileged command it calls -- window-pixel
capture, AX-based input injection, network impairment) must follow, stated
here verbatim so related cockpit work (#255, #257, #261, #262) implements it
consistently:

> Every `start_test_cockpit` call preflights all required grants via
> non-prompting APIs first; on any miss it returns `INFRA-FAIL: run
> scripts/cockpit-setup.sh` immediately and never calls a prompting code path
> (e.g. `remote_control.rs`'s `prompt_accessibility()`) during an actual run.

The reusable helper for this is `test_cockpit::preflight_or_refuse(&AppHandle)`
(`apps/desktop/src-tauri/src/test_cockpit/mod.rs`, gated behind the
`cockpit-privileged` Cargo feature -- see below). It checks for the
`cockpit-setup.sh` marker file and returns the exact `INFRA-FAIL: run
scripts/cockpit-setup.sh` string on a miss. A run must never be interrupted or
derailed by a permission dialog or sudo prompt mid-run -- if you are adding a
privileged cockpit command, call this helper before doing anything else in
that command, full stop.

### `cockpit-privileged` feature flag + QA build channel

#### Direct primary for `SHARE-N2N` (#313)

The native-to-native cockpit primary is a debug QA artifact, not the customer
release binary. Build it through the supported path:

```sh
apps/desktop/scripts/build-cockpit-primary.sh
```

That script builds `target/debug/desktop` with `cockpit-privileged` and asserts
the QA runtime policy: full Xcode supplies compatibility archives at build
time, while the raw binary has exactly the OS `/usr/lib/swift` Swift rpath at
launch. It rejects CommandLineTools and Xcode toolchain *dynamic* rpaths. It
never accepts or documents a `DYLD_*` runtime wrapper. To explicitly exercise
a GUI launch after TCC setup, use `--verify-direct-launch`; it starts the
owned process with `env -i`, waits for the owner-only autotest socket, captures
the process's resolved Swift mapping with `lsof`, rejects duplicate Swift
Objective-C class/dyld failures, and tears the process down.
The direct launch is a linkage proof only, not a Screen Recording or
Accessibility proof — run `cockpit-setup.sh` and the normal preflight after
every rebuild. For a default/release artifact, assert the inverse with
`build-cockpit-primary.sh --assert-artifact <path-to-binary>`; it fails if a
toolchain runtime path is present. Run the same policy for the receiver with
`apps/desktop/scripts/build-test-peer.sh --verify-direct-launch`; both binaries
must pass before rerunning `SHARE-N2N`. The setup script builds both through
this policy but never grants TCC: re-run its non-prompting preflight after each
final QA rebuild.

Only signed QA and release commands use the trusted
`scripts/run-with-source-provenance.sh --require-clean` mode. It rejects a
non-clean caller, materializes canonical HEAD in an isolated checkout, installs
lockfile dependencies there, and builds from that checkout rather than caller
untracked inputs. Standard local/cloud CI and direct `cargo` invocations remain
explicitly `unverified`; they validate code but do not produce incident-ready
artifacts. The wrapper seals all tracked and nonignored-untracked content, and
fails before downstream publication if the isolated source or caller's final
state changed. Its state-bound environment marker is an operational handoff to
`build.rs`, not authentication against a hostile builder. The invoked build
command, installed dependencies, compiler/toolchain, and other same-UID
processes are trusted: the final fingerprint catches persistent writes, but a
malicious actor that mutates isolated input, consumes it, then restores the
bytes before the final check is outside this operational provenance boundary.
Hermetic build-command and same-UID sandboxing are separate supply-chain
hardening, not a claim made by this marker.

During `SHARE-N2N`, the separate cockpit status surface reports native-owned
`PREPARE`, `STARTING`, `CAPTURE_LOCKED`, or `FAILED` state. The captured test
source remains exactly 960x600; the status UI never overlays its calibration,
Gray-code, or sharpness pixels. Keep the source window frontmost through
`CAPTURE_LOCKED`: current WKWebView/ScreenCaptureKit behavior can throttle a
backgrounded source, so the UI does not claim focus may safely be moved away.

A post-filing `/counselors` security review (Gemini + Codex independently)
flagged shipping window-pixel capture, AX-based input injection, and a
sudo-privileged network-impair-script invocation inside a notarized,
customer-distributed app (which has `csp: null` in `tauri.conf.json` today) as
a real attack-surface/notarization-heuristic risk if ever reachable via a
future IPC/XSS bug. The resolved design, introduced in #253 and used by
subsequent cockpit work:

- **`cockpit-privileged`** is a new Cargo feature
  (`apps/desktop/src-tauri/Cargo.toml`), mirroring the existing
  `#[cfg(any(debug_assertions, feature = "autotest"))]` pattern used for the
  `autotest` module in `lib.rs`. The three privileged capability classes
  (window-pixel capture, AX-based input injection, network-impairment script
  invocation) must all be compiled behind this feature so a **standard
  customer-distributed build has zero compiled code path to any of them** --
  not merely a runtime-disabled one. Verify this with a symbol spot-check
  after a plain build, e.g.:
  ```sh
  cd apps/desktop/src-tauri
  cargo build --locked
  nm target/debug/libdesktop_lib.a | grep -i test_cockpit   # -> no output
  ```
- A **separate internal/QA build channel** is how the feature actually gets
  enabled: its own Tauri `identifier`/build config (a `TAURI_CONFIG` override,
  the same mechanism the test-peer binary above uses, e.g.
  `TAURI_CONFIG='{"identifier":"com.petal.app.qa"}'`), built through the
  **same** signing/notarization pipeline documented in `docs/RELEASING.md` --
  not a separate, less-trusted pipeline. Building the QA channel adds
  `--features cockpit-privileged` to the `cargo`/`tauri build` invocation;
  everything else (Developer ID signing identity, notarization, stapling)
  matches a normal release build exactly.
- **REQUIRED: build the frontend with `PETAL_INCLUDE_DEV_ROUTES=1`.**
  `svelte.config.js` strips `routes/dev/**` from a normal `npm run build`, so
  without this env var the SHARE-N2W-Q native test-pattern window
  (`WebviewUrl` = `dev/test-pattern.html`) 404s to the SPA fallback and the
  shared window renders frozen/static content (delivered <1fps). Any cockpit
  build -- `cargo build`/`tauri dev`/`tauri build` -- must run its frontend
  build as `PETAL_INCLUDE_DEV_ROUTES=1 npm run build` so
  `build/dev/test-pattern.html` is emitted and embedded. (For a raw
  `cargo build` after changing that route, also re-embed with a
  `touch src-tauri/build.rs`.)
- **SHARE-N2W-Q is a delivered-LIVENESS gate, not a 30fps gate.** It shares
  Petal's OWN WKWebView test-pattern window; macOS throttles self-captured
  WebView content (JS timers + the SCK raw stream), so it delivers via
  <=10fps snapshot-pull. The scenario asserts the native->web pipeline is live
  (received at the source resolution, frames advancing), not raw fps. Real
  third-party app windows capture at full fps -- SHARE-W2N-Q proves the 30fps
  media path in reverse.
- **Belt and suspenders, not either/or**: even inside the QA build channel,
  the privileged capabilities stay inert -- refuse to execute -- unless
  `cockpit-setup.sh`'s one-time local marker file is present, checked via
  `test_cockpit::preflight_or_refuse(&AppHandle)`. A QA build with the feature compiled
  in but no local setup marker behaves identically to a customer build from
  the caller's point of view: `INFRA-FAIL: run scripts/cockpit-setup.sh`.
- This scaffolding must be in place before the Settings-UI button and
  launch-param entry points are wired up (#258) -- those entry points are
  QA-build-only surfaces, not present in a standard customer build at all.

### Test Cockpit entry points: Settings + launch parameters (#258)

The Test Cockpit has exactly two operator entry points, and both are
**QA-build-only** behind `--features cockpit-privileged`:

- **Settings UI**: a `Test cockpit` section appears in Settings only when
  `get_build_info().cockpitPrivileged` is true. A standard customer build
  returns `false`, so the section is absent. The section supports Quick / Full
  / Soak tier selection, optional comma-separated scenario IDs, a Run button,
  live `test-progress` updates, pass/fail summary text, an always-visible
  skipped-list when scenarios are skipped, and an `Open results folder` button
  for `~/Library/Logs/Petal/test-runs/<timestamp>/`. Quick/Full/Soak are three
  journey-level presets (`Settings.svelte`, filtered by priority/depth), not
  the full tier list: four further opt-in tiers exist on the raw
  `ScenarioSpec` table (`test_cockpit/mod.rs`'s `SCENARIO_TABLE`) --
  `native` (SHARE-N2N), `multi-display`, `gap` (targeted regression oracles),
  and `ui` -- reachable only via the comma-separated scenario-ID field above
  or `--test-case=<scenario-id>` below, not this dropdown.
- **Saved-run results viewer**: the same Settings section lists saved runs from
  `~/Library/Logs/Petal/test-runs/<timestamp>/`, loads run details via
  `list_test_cockpit_runs` / `get_test_cockpit_run`, and shows the scorecard,
  readable event timeline, and artifact rows inline. Screenshot artifacts
  render as an image gallery; `.mov`/`.mp4` video artifacts and `.m4a` audio
  snippets render with native media controls. Inline previews are loaded through
  the confined `get_test_cockpit_artifact_data_url` command, which only accepts
  direct child run directories, relative artifact paths under that selected run,
  supported image/video/audio extensions, and files no larger than the inline
  preview cap.
- **Launch parameter**: a QA build parses `--test-case=<ids|tier>` and
  `--test-case <ids|tier>` from process argv before the Tauri builder is
  constructed. `PETAL_TEST_CASE=<ids|tier>` is the env-var equivalent when no
  argv value is present. If Petal is already running, the single-instance
  callback forwards a second launch's `--test-case...` argv to the running
  instance instead of discarding it.

Examples:

```sh
cd apps/desktop/src-tauri
cargo run --features cockpit-privileged -- --test-case=quick
PETAL_TEST_CASE=SHARE-W2N-Q cargo run --features cockpit-privileged
cargo run --features cockpit-privileged -- --test-case SHARE-W2N-Q,SHARE-N2W-Q
```

Launch-param runs are intended for CI/headless use: they log and print the
results directory as `PETAL_TEST_COCKPIT_RESULTS_DIR=...` and exit `0` only
when the cockpit summary is `passed`; failed/cancelled/infra-fail outcomes exit
non-zero. The #257 engine is the execution source of truth; Settings,
launch-param runs, and the dev wrapper all delegate to the same Rust path.

### Manual-verification artifacts (#265)

Cockpit runs emit artifact events in `run.jsonl` so the viewer can associate
each saved file with the exact step/verdict it documents:

```json
{"kind":"artifact","payload":{"type":"screenshot","path":"artifacts/...png","stepId":"verdict","tMs":1234}}
```

Screenshots are retained alongside structured results. Video and audio files
can grow quickly during Full/Soak runs, so the engine prunes only `video` and
`audio` artifacts referenced by `run.jsonl`; `run.jsonl`, `scorecard.json`, and
screenshot artifacts are preserved. The prune pass runs after each cockpit run,
emits an `artifact-retention` event with scanned/kept/pruned/skipped counts, and
is bounded by the configurable env vars below. The retention resolver confines
artifact paths under the local test-runs root before removing anything.

There is intentionally **no** `petal://test/...` deep-link entry point. Test
launch must not be remotely triggerable by a webpage, chat message, or other
URL surface. If a test deep link is ever reconsidered, it needs an in-app
confirmation click and should be filed separately.

### TCC grant survival across rebuilds (empirically settled, #253)

There was a live contradiction between this file's sibling docs about whether
a Screen Recording/Accessibility grant survives a `cargo build` rebuild of
`target/debug/desktop`. It's settled: **yes, it survives** -- see CLAUDE.md's
"Use `npm run dev:clean`" section for the full empirical test (rebuilt twice,
recorded three distinct ad-hoc CDHashes, confirmed `GRANTED` after each
rebuild via the same non-prompting preflight the app itself uses). Only a
`tauri build` bundle re-sign invalidates the grant, not a plain `cargo build`
relink.

### Multi-participant validation (`docs/MULTI_PARTICIPANT_TEST_PLAN.md` retired)

That doc predated the Test Cockpit build-out and was still labeled "DRAFT v2"
with no updates since. Multi-participant validation now has two current,
actively-maintained owners instead: the Cockpit's `MULTI-3` scenario
(automated, native tier) for the mechanical join/roster/independence proof,
and issue #28 (the running live-validation tracker) for anything needing a
human end-to-end pass. Don't recreate a third parallel tracking doc for this.

### SHARE-W2N-Q walking skeleton and Rust engine (#254/#257)

**Status: the SHARE-W2N-Q proof is implemented, and the
`src-tauri/src/test_cockpit/` engine now owns the Quick-tier scenario table,
fresh `rctest-*` room creation, `p-cockpit-*` identity, Chrome launch,
diagnostics assertions, cleanup verdict, and redacted `run.jsonl` /
`scorecard.json` output.** The web harness currently has only the
`share-w2n-q` unattended driver; the other Quick IDs are registered in the Rust
table and produce honest infra-fail artifacts until their web/native
self-drivers are added.

What it does:
- `dump_metrics` -- a new autotest-socket command (`apps/desktop/src-tauri/
  src/autotest.rs`) exposing the existing `DiagnosticsState::snapshot()`/
  `journal()` (already computed for the Network Cockpit UI) plus a bounded
  journal tail. Read-only, synchronous, non-privileged.
- `web-harness`'s `__petalHarness.cockpitAutoScenario.join(code)` /
  `.sharePattern()` (`src/
  cockpit.ts`, wired in `src/controls.ts`/`src/main.ts`) expose the same
  join/publish paths the interactive UI uses as plain callables, plus a
  `?auto=<scenarioId>` URL param that runs an unattended join -> self-check
  -> sharePattern step list with no CDP required for that flow itself.
  Each step self-reports over a new `petal.cockpit` LiveKit data topic
  (mirrors `petal.pipeline-stats`); the native side (`cockpit_topic.rs`)
  receives and journals it; verdict consumption is not part of that topic.
- The `__petalHarness.cockpitAutoScenario` namespace is URL-triggered and
  self-driving via `?auto=<scenarioId>&code=<accessCode>`, reporting results
  over the `petal.cockpit` LiveKit data topic. The separate
  `__petalHarness.remoteControl` namespace is CDP-driven and imperative
  (click/drag/type automation), reporting results over raw script stdout.
- `apps/desktop/scripts/cockpit.mjs` is now only a dev wrapper around the Rust
  engine: it runs `cargo run --features cockpit-privileged --
  --test-case=<selector>`. Run it with `node apps/desktop/scripts/cockpit.mjs
  quick` or `node apps/desktop/scripts/cockpit.mjs --test-case=SHARE-W2N-Q`.

**Known caveat -- shared/loaded dev machine can cap headless Chrome's own
encode fps below the 20fps assertion.** Live-validating #254, the fps
assertion timed out with native measuring a steady ~15fps recv track. The
web peer's own `petal.pipeline-stats` self-report showed WHY: its capture
metric (`remoteGrabbed`) measured a healthy 30fps, but its own encode/send
metric (`remoteEncodedSent`) was already down to ~15fps -- and native's
received fps matched that ~15fps exactly, i.e. native faithfully decoded
everything the browser actually sent. The bottleneck was upstream of native
entirely, most plausibly headless Chrome's software video encoder (no GPU
acceleration in `--headless=new`) falling behind under this machine's own
concurrent load from other sessions (many other dev servers/Chrome
instances were running in parallel at the time) -- not a Petal receive/
decode regression. `cockpit.mjs`'s fps-assertion-timeout path now checks
this itself: if the web peer's own self-reported encode fps is already at
or below the threshold and native's received fps matches it, it classifies
**INFRA-FAIL** (with the comparison in the reason string) instead of
TEST-FAIL; a genuine receive-side shortfall (web healthy, native behind)
still classifies TEST-FAIL. The real `fps > 20` bar in the scenario itself
is unchanged -- only the FAIL classification got smarter. Re-run on a less-
loaded machine (or CI) for a real PASS reading of this scenario's actual
number.

## Signed Release Clean-TCC Smoke

The release-only smoke scaffold is:

```sh
scripts/release-smoke.sh --guide-only
scripts/release-smoke.sh --app /Applications/Petal.app --dmg path/to/Petal.dmg
scripts/release-smoke.sh --app /Applications/Petal.app --assert-log
```

Run it against a signed release app, not `tauri dev`. The script verifies the
Developer ID team, hardened runtime, absence of CommandLineTools rpaths, and
optionally the stapled/notarized DMG. It then prints the human-only clean-TCC
steps: reset/revoke Screen Recording and Accessibility on the test Mac, launch
the signed app, grant Screen Recording, relaunch, join a real room, share a
window, request remote control from another peer, and verify first input lands.

After the live pass, `--assert-log` checks `~/Library/Logs/Petal/petal.log` for
the expected permission/share/remote-control markers. Pass `--marker` or
`--markers-file` to add the newer capture heartbeat, pump liveness, or input-drop
markers as those land. A release is not ready until this clean-TCC smoke passes
and the result block records the artifact, macOS version, prompt behavior, share
liveness, remote-control first-input result, and log assertion output.

## A locked screen silently stops capture while every liveness signal stays green

**Measured live, 2026-07-29.** A four-ladder measurement rotation ran to
completion and produced **zero samples**. Nothing errored. The cause: the
screen locked partway through, and under a locked screen ScreenCaptureKit
reports **`stream alive, source not drawing`** for every window — the stream is
genuinely up, it simply never delivers a frame.

Every health signal available said the pipeline was fine. It was fine. It was
also capturing nothing, and those are not the same state.

**Before any long unattended capture run, hold the display awake.** `caffeinate`
is the blunt instrument:

```bash
caffeinate -d -i -s -- ./your-measurement-script.sh
```

**And gate on frames delivered, not on stream liveness.** A run that produces no
samples must fail loudly and early rather than completing with an empty result
set. If a rotation can finish "successfully" with zero rows, its success signal
cannot distinguish working from producing-nothing — the same defect class this
file documents elsewhere, and the reason an hour of measurement was voided
before anyone noticed.

Note this is *not* the same as the display-sleep pause in `resilience.rs`
(#259/#264), which deliberately gates compositor enqueue on the **receiver**
side while a display sleeps. This is the **capture** side, and there is no
equivalent guard.

## A timing-out tool call SIGTERMs the whole process group — including your target window

**Measured live, 2026-07-29, during the #613 ladder rotation.** Two ad-hoc target
windows died *mid-run* and the failure did not look like a harness failure: the
probe reported **"No matching window found"**, which reads as an app bug. One
whole batch was corrupted before anyone noticed.

**Cause.** The target window was launched from one tool call, and a *later,
unrelated* tool call hit its `timeout`. The timeout signals the **process
group**, so it killed the target too. The harness silently shot its own subject.

This is also why ad-hoc target/sentinel windows have repeatedly been found
littering the user's screen: the leak risk and the correctness risk are the
**same bug**, seen from two ends.

**The fix, and it is not "remember to clean up".** Every batch script must
**own** its target and kill it on all exit paths:

```bash
"$TARGET_BIN" --width 842 --height 468 --fps 60 --seconds 1200 &
TARGET_PID=$!
trap 'kill "$TARGET_PID" 2>/dev/null' EXIT INT TERM
```

Prefer a **script-owned window per batch** over one long-lived window shared
across batches. A long-lived window is tidier in principle and was tried; in
practice it is the one that survives its owner. A script-owned window that
provably dies beats a shared one that provably didn't.

Give the target a **self-terminating `--seconds` bound as a backstop**, not as
the mechanism. If the `--seconds` timer is what actually reclaims your windows,
you have a leak with a long fuse.

**Verify teardown, don't assert it.** Finish a measurement session with an
actual `pgrep` and report its output — including `livekit-server`, which has no
self-terminate at all. "Should be clean" has been wrong often enough that it is
not evidence.

## Budget your live runs: capture the wire before you spend a suite

**Measured cost, 2026-08-14.** One `rc-live-suite.sh` run is ~10 minutes, a
Petal GUI launch, a Chrome launch, a TextEdit launch, and a `dev:clean` that
kills any other agent's Petal instance. It is the most expensive diagnostic in
this repo. Treat it as a *verification* step, not an *investigation* one.

The #802 cycle is the cautionary example. Three full suite runs were spent, and
the first fix — reasoned out from a static read of the browser's grant gate —
was aimed at the wrong half and changed nothing live. What actually settled it
was a **passive CDP probe** that recorded the raw inbound status packet while a
run happened to be in flight:

```
grantTokenType     "string"     <- the token IS sent; the whole leading
resultCapability   version 2         hypothesis was wrong
targetKind         <absent>
shareInstanceId    <absent>
hostCapabilities   <absent>
```

Three facts, ~90 seconds, no extra GUI launch. They inverted the diagnosis. The
same probe run *first* would have saved two suite runs and the second fix would
have been the first.

**The rule: never run the full matrix to learn one fact.** Before spending a
run, ask what single observation would discriminate between your hypotheses,
and get *that*. In rough order of cost:

| Cost | Tool | Answers |
|---|---|---|
| seconds | `grep ~/Library/Logs/Petal/petal.log` | what the host decided, and when |
| seconds | a Rust unit test on the pure function | is the logic right in isolation |
| ~90s | passive CDP probe against a live page | what is actually **on the wire** |
| ~10min | `scripts/rc-live-suite.sh` | does the whole path work end to end |

A passive probe *observes* — it installs a `dataReceived` listener and never
calls `api.request()`, so it can run alongside a suite without perturbing it.
An active probe that issues its own request will race the suite and corrupt
both. Keep probes in the scratchpad; they are debugging tools, not deliverables
(COURSE_CORRECTION rule 1).

**Corollary — a static trace is a hypothesis, not a finding.** Reading the code
and following the data flow tells you what *can* happen. Only a measurement
tells you what *did*. Two independent things were both true of #802 and only
one was the defect; no amount of re-reading the gate could have distinguished
them, because the deciding fact (`shareInstanceId` is never published by a Mac
host) lived three modules away in `set_shared_window_info_for_generation`.

**And clean up after every run, including the failures.** A live run launches
GUI apps that outlive the shell that started them. `scripts/rc-live-suite.sh`
now refuses to start when a foreign `desktop` is running
(`assert_no_foreign_petal`, tested both directions by
`scripts/test-rc-suite-instance-guard.sh`), tears down descendants it recorded
before signalling (#798), and sweeps its sacrificial TextEdit and sentinel
windows. Verify by PID afterwards regardless — `pgrep -f` matches your own
command line and will lie to you in both directions.

## Running `scripts/rc-live-suite.sh` from an AI-agent shell (thirteen hiccups, all fixed or worked around)

**Measured live, 2026-08-14**, running the suite from an agent session (not a
human terminal) against a busy shared Mac (4+ concurrent Claude sessions, load
averages 9–17). Every one of the following looked like a Petal defect on first
sight; none of them were the product. Recorded here so the next run — human or
agent — spends minutes on these, not hours.

**1. `run_in_background: true` does not exempt a command from the agent tool's
own default timeout.** The Bash tool this session used defaults to killing any
command after 120s unless you pass an explicit `timeout` — and that default
applies *regardless* of `run_in_background` or an internal `timeout 5400`
inside the script itself. Every attempt died ~120s in, always at the same
point (right after step 6 started), which looked exactly like a suite bug
until a plain canary background job (`while true; do …; sleep 3; done`, not
Petal-related at all) died silently the same way. **Fix: always pass an
explicit `timeout` (max 600000ms/10min) sized to what the command actually
needs** — the agent-tooling equivalent of "always wrap `cargo test` in
`timeout`" from this same doc.

**2. A cold build eats the budget before the interesting part even starts.**
`rc-live-suite.sh` builds Petal as step 4 of 6; a from-scratch `cargo build` on
this repo runs 2–4 minutes, which combined with hiccup #1's ceiling can burn
the whole run before step 6 (the actual matrix) begins. **Fix: pre-warm
`npm ci`/`cargo build` as their own separate, generously-timed background
calls first**, so the suite's own run only needs to relaunch an
already-compiled binary.

**3. Killing the suite's tracked wrapper PID does not kill everything it
started — reproduced three separate times in one session (#798).** `own_process
"$!"` in `owned-process-cleanup.sh` only records the top-level `npm run
dev:clean` PID; the actual `target/debug/desktop` binary is a grandchild
spawned further down the `npm` → `dev.sh` → `tauri dev` chain, and does not
reliably die when the top PID gets SIGTERM'd. `release_owned_processes` prints
`"nothing this script started is still alive"` even when that's false. **Every
time this suite reports a failure, confirm with your own `pgrep -fl
"target/debug/desktop"` before starting the next attempt** — don't trust the
cleanup line. Tracked as #798; not yet fixed.

**4. The `--live` wrapper buffers ALL child output until the child exits — zero
incremental visibility during the actual test matrix.**
`remote-control-local-loopback.mjs` accumulates the scenario child's
stdout/stderr into strings via `on('data', ...)` and only `console.log`s them
line-by-line after `child.once('close', ...)` fires. From outside (tailing the
suite's log file), a live 29-case run and a genuine multi-minute hang are
*indistinguishable* — nothing prints either way until the child exits. **Do not
conclude "stalled" from log silence alone.** Check the actual scenario
process's accumulated CPU time (`ps -p <pid> -o pid,etime,time`) — real work
burns CPU; a true hang sits at ~0:00 for the entire elapsed time. This gap is
not yet filed as its own issue; a real fix would stream `child.stdout` through
to `process.stdout` live in addition to accumulating it for parsing.

**5. Confirmed live: an unbounded `spawnSync` can and does hang 40+ minutes on
a busy shared Mac (#799, fixed).** Using hiccup #4's CPU-time check surfaced a
genuine hang: `defaults write com.apple.TextEdit …` (part of the TextEdit-setup
step) sat blocked for 44:44 with near-zero CPU, almost certainly `cfprefsd`
contention under concurrent load. `remote-control-scenario.mjs`'s own
`osascript()` helper already documents and guards this exact failure class for
its `execFileSync` calls (`timeout: 5000`) — the fix (PR #800) just extended
the same pattern to the three `spawnSync` calls that had been missed. If a
step involving `defaults`, `open`, `pkill`, or any other synchronous
`child_process` call ever wedges again, check for a missing `timeout` first.

**6. `setsid` does not exist on macOS, and the agent tool's timeout still
reaps a plain `&` background job.** Hiccup #1's fix (an explicit `timeout`)
caps out at 10 minutes, which is shorter than a cold-build run and *much*
shorter than any idle-behaviour test. `nohup setsid ...` fails outright
(`nohup: setsid: No such file or directory`). What works is a Python
double-fork that calls `os.setsid()` itself, so the run lands in its own
session and survives the tool call that started it:

```python
import os
pid = os.fork()
if pid == 0:
    os.setsid()
    fd = os.open(LOG, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
    os.dup2(fd, 1); os.dup2(fd, 2)
    os.dup2(os.open("/dev/null", os.O_RDONLY), 0)
    os.chdir(REPO)
    os.execv("/bin/bash", ["/bin/bash", SCRIPT, LOG])
else:
    print("DETACHED_PID=%d" % pid)
```

Then poll the **artifact**, not the process: `until [ -f /tmp/rc-results.json ]`.
That is also hiccup #4's answer -- the wrapper buffers all child output, so the
results file appearing is the first honest progress signal available.

**7. Reuse-if-present must key on the PORT, not the process name.** The suite
skips starting `livekit-server` when `pgrep -f "livekit-server --dev"` matches.
A server that is *mid-shutdown* still matches, and then refuses the connection:
Petal logs `join_room(...) failed -- failed to connect to LiveKit room`, every
later `share`/`share_matching` returns `not currently in a room`, and the run
proceeds to test nothing for its full duration. Check `lsof -iTCP:7880
-sTCP:LISTEN` instead, and wait for the port to listen after starting the
server. Any one-off harness should also **fail fast on a join error** rather
than idling out its timer against a session that never joined.

**8. `share_matching` refuses an ambiguous selector, and says so precisely.**
`{"cmd":"share_matching","app_name":"Finder"}` produces `share_matching matched
2 windows; use a unique title_contains or pid selector:
6200:Finder:Some("tmp"), 5607:Finder:Some("ring360")`. Add `title_contains`.
Gate on `"shared":true` in the result, not on the `"ok":true` envelope, which
can wrap a refusal.

**9. A `granted:false` from the controller has two very different causes, and
the default message could not tell them apart.** `waitForActiveStatus` now
reports `api.active()` and the recent status metrics alongside the grant, which
is what identified #808 in a single run:

| what the enriched error shows | meaning |
|---|---|
| `active=null` | the controller has no session at all -- it was cleared (#808's shape). No `grant REJECTED` will have been logged either, because that diagnostic sits inside the same `active &&` guard. |
| `active={...}` with `grantToken:null` | a session exists but adopted no token -- the #802/#580 family. |

**Do not re-add a token field to that probe from `metrics.statuses`.** It
records no `grantToken` (`harnessApi.ts`), so any such check reads false for
every status and cannot distinguish "arrived without a token" from "arrived
with one". That exact field was added, believed, and nearly written up as a
root cause before being caught.

**10. EVERY `git push` and every `ci-local.sh` run silently destroys the
cockpit-privileged binary.** The pre-push hook and the CI script both run a
default-feature `cargo build` at the same `target/debug/desktop` path that
`build-cockpit-primary.sh` writes. The clobber is invisible until you launch
`--test-case=...` and the app just... sits there. **The failure signature is
exact:** the log shows `main window revealed by the 2500ms fallback -- the
frontend never reported first paint` and, decisively, NO
`test-cockpit: launch-param trigger` line — because the trigger is behind
`#[cfg(feature = "cockpit-privileged")]` and the binary no longer has the
feature. This bit the same agent session **three times in one day** (after a
push, after a gate run, after another push). Rule: **re-run
`build-cockpit-primary.sh` after ANY push or ci-local run, before ANY cockpit
launch** — it is incremental (~2-4 min warm) — and if a cockpit app hangs at
startup, grep for the trigger line before debugging anything else.

**11. Verifying a web-harness change against prod LiveKit WITHOUT deploying:
build the prod bundle and serve it locally.** The cockpit runbook's warning
that "local-serving the harness is NOT a shortcut" applies to `npm run dev`
(which mints tokens against a local livekit). A **prod build** served
statically keeps prod backend/LiveKit wiring:

```sh
cd web-harness && npm run build && npx vite preview --port 4173 --strictPort &
PETAL_HARNESS_URL=http://localhost:4173 ./src-tauri/target/debug/desktop --test-case=AUD-01
```

Sanity-check the served bundle first (`curl -s localhost:4173/ | grep -oE
'/assets/meeting-[^"]+\.js'`, then grep that JS for `petal.cockpit`). This is
how the #787 RED experiment ran both arms against prod in under two minutes
per run, with zero deploys. Deploy for real once the change is proven.

**12. HEADLESS CHROME CANNOT DECODE REMOTE AUDIO — and it fails silently, as
silence.** With no audio output device it never runs the decoder for a
subscribed audio track: `packetsReceived` and `bytesReceived` climb normally
(121 B/packet of real content), `packetsDiscarded` and `concealedSamples` stay
0, and `totalSamplesDuration` advances on the playout clock — while
`totalSamplesReceived` and `jitterBufferEmittedCount` stay **0**. Every signal
looks like a healthy stream that decoded to silence. A `MediaRecorder` capture
of the same track reads zero for the same reason. Measured 2026-08-15: a native
440Hz tone that a real Chrome renders at rms 0.35, and a native subscriber
hears at RMS 11528, read **exactly 0.0000** headless — which produced a P0 bug
report (#821) against a working product. No flag fixes it
(`--use-fake-device-for-media-stream` and `--alsa-output-device` were both
measured still-blind). **Run any audio-receiving browser peer HEADED**, positioned off-screen
(`--window-position=-3000,0`) to keep it out of the way -- but note a headed
launch still activates Chrome and briefly takes key focus; no flag prevents
that, so don't schedule one while you need the keyboard. The
cockpit now does this automatically for `AUD-N2W`, and
`measureCockpitRemoteAudio` refuses to report silence when
`packetsReceived > 0 && totalSamplesReceived == 0` — "could not listen" is an
INFRA failure, never "heard nothing".

**13. Petal joins MUTED, so an automated audio run measures silence unless it
unmutes.** The session applies the persisted (muted) mic state immediately
after publish, and a muted track is indistinguishable from a broken pipeline at
every receiver. Rigs with no UI to click unmute set
`PETAL_AUDIO_PUBLISH_UNMUTED=1`, which publishes unmuted AND refuses the
session's join-time mute. (The cockpit's `AUD-N2W` instead unmutes through the
real `SessionState` path the menubar toggle uses, which is the more faithful
exercise; both are correct, and unmuting after publish was measured to restore
audio fully.)

**14. An acoustic speaker oracle must baseline ITSELF before any Petal
verdict, and must score band energy, never a single frequency bin.** A
speaker-side check (play a tone through the app, listen with the Mac's own
microphone) "reproduced" a deaf native receiver 5/5 while the speakers were
audibly playing — the metric was an exact-440Hz Goertzel bin, and the
ffmpeg/avfoundation capture chain lands the tone at ~430–435Hz (resample
clock skew), so the exact bin reads a fraction of the energy. The same metric
scored a bare `afplay` of the tone as "silent", which is the tell: an oracle
that cannot hear `afplay` proves nothing about Petal.
`scripts/verify-speaker-playout.sh` (the #787 join-into-active-meeting gate)
bakes in the rules: an `afplay` positive control and a noise-floor control run
FIRST and fail as INFRA, and the verdict is 415–475Hz band energy against the
rest of the spectrum (noise scores ~0.01, real tone ≥2).

**Also worth knowing, not bugs:** `PETAL_REMOTE_CONTROL_SHARE_READY_TIMEOUT_MS`
(default 8000) and `PETAL_REMOTE_CONTROL_WEB_JOIN_TIMEOUT_MS` (default 15000)
are both explicitly documented in `remote-control-scenario.mjs` as
environment-sensitive on a loaded machine — bump both (e.g. to 30000/45000) as
a first move if either times out on a busy box, before assuming a product
regression.

## Reproducing a genuinely static capture source (#806) -- three things that look static and are not

**Measured 2026-08-14**, validating #806 (a static shared window must not trip
the 45s wedge watchdog). Producing the failing condition took three attempts,
because the condition is *SCK delivering samples with NO image buffer, status
Idle* -- and almost nothing on a desktop actually stops drawing:

1. **A text editor is not static.** An open TextEdit document blinks its
   caret, which redraws the window ~2x/sec; capture-diag showed
   `~29.3fps status=Some(Complete) dirty=1rects/1107328px`. Raw silence never
   got near 45s.
2. **A backgrounded window is not static either.** A backgrounded Finder
   window still delivered `~29.4fps` with a full-window dirty rect and an
   unchanged hash. macOS keeps compositing a captured window; losing focus
   changes nothing.
3. **What works: share the window, then HIDE its app** (`System Events -> set
   visible of process "TextEdit" to false`, or Cmd+H). The window leaves the
   composited scene and SCK genuinely stops delivering frames. Occlusion by
   another window also works but is fiddlier to script.

The verified sequence, from the #806 run (share a TextEdit doc, hide the app,
wait 390s, touch nothing):

```
20:55:08  raw capture idle for 45.3s -- source not drawing, stream healthy, holding last frame (no restart)
20:55:31  capture-pull: snapshots=436 changed_pushed=1 last_hash=3e4cdf06... (raw silent 67.7s)
20:58:54  raw capture idle for 271.3s -- ... (no restart)
20:59:23  no raw ScreenCaptureKit frames for 300.3s (... restarting)   <- the hard backstop, exactly once
```

Pass criteria: `IdleHealthy` holds at 45/90/135/180/226/271s, **zero**
restarts before 300s, **at most one** defensive restart at the 300s backstop,
**zero** `pump recovery circuit open`. The pre-fix behaviour was restarts at
45/90/135s and a dead share at ~2m15s.

**Known consequence of the hidden-app method (#810):** the 300s defensive
restart cannot enumerate a hidden window in SCK's shareable list, classifies
it as *closed*, and stops the share. Until #810 is fixed, a hidden-source run
ends at ~305s with a teardown that is #810's bug, not #806's -- score
everything before the backstop, and expect the teardown after it.

There is deliberately no committed script for this: it is a verification
recipe for one fix, not test infrastructure (COURSE_CORRECTION rule 1). The
autotest socket gives you everything needed: `share_matching` with
`app_name` + `title_contains`, then AppleScript to hide the process, then
grep the petal log for the four signatures above.

## Example Probes

The Rust example probes live in `apps/desktop/src-tauri/examples/`. See `apps/desktop/src-tauri/examples/README.md` for the full list and run recipes.

In short:

- `capture_probe` - ScreenCaptureKit window capture without LiveKit.
- `publish_probe` - capture a real window and publish H.264 to LiveKit.
- `subscribe_probe` - subscribe to a published share and measure frame/latency stats.
- `compositor_probe` - subscribe and paint decoded H.264 frames into a native display layer.
- `audio_probe` - synthetic 440 Hz audio publish/subscribe round trip.
- `mic_capture_probe` - real microphone device capture and outbound RTP stats.
- `mint_token` - mint a LiveKit token for manual harness use.
- `bare_window_probe` - minimal AppKit window diagnostic.
- `camera_cadence_probe` - synthetic-source webcam publish/subscribe cadence; built for the cross-architecture tier below.
- `share_lifecycle_probe` - real-SFU share lifecycle: open-at-source-size, survive-the-republish, late-joiner discovery.
- `hold_last_frame_probe` - the native no-black-frame pixel gate; see below.

Most probes need `LIVEKIT_URL`, `LIVEKIT_API_KEY`, and `LIVEKIT_API_SECRET`. Some load them from `apps/desktop/.env`; `audio_probe` and `mic_capture_probe` require direct process env.

### Receiver start-order diagnostic (#613)

`apps/desktop/scripts/run-issue613-receiver-start-order.sh` exposes two separate
matrices: `--matrix synthetic` isolates receiver order behind an example-only
fixed 30fps NV12 source, while `--matrix real` confirms against ScreenCaptureKit
capture. `--matrix both` runs them serially. Both use the fixed four-pair
`early/late`, `late/early`, `early/late`, `late/early` rotation plus one
`PETAL_PLAYOUT_DELAY_MS=400` positive-control arm. The synthetic source still
uses `RoomConnection::connect_and_publish` and `PublishedTrack::push_frame`, so
the LiveKit metadata, codec, and simulcast path is unchanged. The runner starts
only owned, bounded processes: a local SFU, a deterministic nonactivating target
when real capture is selected, and the paired production-path probes. Build
once first:

```sh
cd apps/desktop/src-tauri
cargo build --locked --example publish_probe --example subscribe_probe
../scripts/run-issue613-receiver-start-order.sh --matrix synthetic
../scripts/run-issue613-receiver-start-order.sh --matrix real
```

For the real matrix, the nonactivating AppKit target derives its decorated
outer frame and content rectangle from the selected display's backing scale and
titled-window decoration so ScreenCaptureKit frame #1 is exactly 1600×900
physical pixels. The publisher aborts before LiveKit publication on a mismatch,
and the runner requires `CAPTURE_RASTER_VERIFIED 1600x900` before accepting
publisher startup. This keeps Matrix B physically comparable to the fixed
1600×900 synthetic source instead of treating AppKit points as capture pixels.

Both probes read the identical absolute publisher-age 16–28 second boundary;
the late receiver starts at publisher age eight seconds. Publisher pushed-frame
and capture-slot-overwrite validity comes only from 16s/28s counter deltas, not
whole-run averages. Receiver sample inclusion uses decoded-callback
`receive_us` wall-clock. Embedded sender `capture_us` reports source/capture age
and feeds the capture→decode lower-bound distribution; it does not select the
window. Each arm is valid only with 11.5–12.5 second receiver and publisher
delta windows, matching scheduled boundaries, at least 300 aligned timestamped
frames, 25–35 decoded and publisher-pushed fps, zero
`end_to_end_publisher_frame_gaps`, zero `receiver_frames_dropped`, no missing
timestamps, capture-slot overwrites at or below 1% of pushed frames, and no
sender CPU/bandwidth limitation. Capture-slot overwrites happen before frame-id
assignment and therefore cannot explain publisher frame-id gaps.

The control must apply its production setter and move both jitter target and
the capture-callback-to-decoded-callback p50 lower bound by at least 100 ms.
That p50 is explicitly **not** glass-to-glass latency. The hypothesis is
supported within one matrix only when all four pairs have `early > late` for
both metrics and the median jitter-target difference is at least 32 ms. Mixed
signs or a smaller median mean **FALSIFIED**, not a prompt to patch the product.
A product conclusion is prohibited unless both synthetic and real-capture
matrices are valid and `SUPPORTED`; a single or disagreeing matrix records
`NO_PRODUCT_CONCLUSION`. Raw CSV, logs, per-arm JSON, aligned publisher
cadence/overwrite evidence, per-matrix `verdict.json`, and
`combined-verdict.json` live under the printed Petal-owned evidence directory
in `/private/tmp`.

### Presentation-inclusive latency matrix (#613, deferred live operator run)

`apps/desktop/scripts/run-issue613-presentation-latency.mjs` measures from a
source pattern's observed presentation to its destination pattern's observed
presentation on one physical display. It is deliberately distinct from the
receiver start-order metric above: the latter ends at decoded callback and is
not glass-to-glass.

Before it can open a cell, the coordinator enumerates display IDs, AppKit/CG
bounds, and backing scales, then chooses one display that fits two **separate
640×360 presentation crops** plus a 40-pixel margin: source `(40,40)` and
destination `(720,40)`. These are observed presentation crops, not transport
source resolution: the native capture/publisher contract remains 960×600.
It explicitly positions the owned Chrome content and the nonactivating native
compositor window, converts the compositor's top-origin pixel crop into an
AppKit frame, and requires the observer to use that exact display ID and the
concrete source/destination window IDs. It fails before a cell if the selected
layout is unavailable, either crop/window is out of that display, the crops
overlap, or Chrome's actual CSS video rectangle is not exact.

`--direction` accepts `n2w`, `w2n`, or `both` (the default). While native→web
remains blocked at its exact direct-WindowCapture first-frame preflight—the
failed apparatus delivered no accepted frame, and the new counters will
separate no-image-buffer SCK samples from layout/format/stream-error paths—a
bounded web→native run is separately actionable:

```sh
node apps/desktop/scripts/run-issue613-presentation-latency.mjs \
  --direction w2n --load both
```

W2N never starts a native source, `publish_probe`, or capture preflight. It
still performs its visible browser-pattern source, native compositor control
and baseline, idle/CPU-50 cells, observer validation, positive-control gate,
and owned-process lease cleanup. A W2N result is directional evidence only;
it cannot clear the N2W ScreenCaptureKit blocker.

The coordinator owns every process it starts and writes `owned-process-lease.tsv`
with PID/PGID, working directory, and log path. It runs native→web and
web→native at idle and a bounded one-core 50%-CPU worker, requiring 120 unique
post-warmup paired Gray-code generations per cell. ScreenCaptureKit captures
one uniquely selected display from concrete source/destination SCWindow IDs and
decodes the two calibrated crops in memory after validating their CG/SCWindow
physical-coordinate transforms; only timing,
counter, and local CSV/log summaries are retained—never raw pixels or Sentry
events. A valid cell has non-overlapping in-display crops, 25–35 paired fps,
zero incomplete-frame/status errors, zero post-ready decode failures, and zero
unpaired destination generations, or counter regressions. The authoritative
result is p95 <100 ms in every idle/CPU cell. Before each baseline, a +200 ms
control must move paired p50 by 150–250 ms: native→web delays the
captured/stamped frame in the example publisher while the actual product remote
video remains visible. That control uses a bounded timestamped FIFO drained in
capture order at capture+delay; any queue overflow is an explicit invalidation,
not a dropped/overwritten sample. Web→native delays compositor enqueue. The CPU worker
reports `process.cpuUsage`/wall utilization and must stay within 45–55% of one
logical core. A failed control or load gate invalidates the direction rather
than being reported as product latency.

The #613 observer remains a full-display ScreenCaptureKit capture; never add a
direct-window fallback. If its candidate snapshot is empty, it emits
`INVALID_OBSERVER_DISPLAY_UNAVAILABLE`, writes no valid CSV/result cell, and
the coordinator saves a zero-cell invalid artifact with the candidate diagnostic
and resume condition. Treat that as an apparatus invalidation, not a product
failure; resume only after `SCShareableContent` reports one matching display.

Build and static-check first:

```sh
cd apps/desktop/src-tauri
cargo build --locked --example publish_probe --example compositor_probe
cd ..
node scripts/run-issue613-presentation-latency.mjs --self-test
swift scripts/presentation-latency-observer.swift --self-test
```

Before any experimental cell, first run the separate capture delivery
preflight against the exact source window. It uses the same direct-window
`WindowCapture` configuration as the publisher but exits before LiveKit
credentials, token minting, or connection:

```sh
cd apps/desktop/src-tauri
cargo run --example publish_probe -- <window-id> --source real \
  --expected-capture-width 960 --expected-capture-height 600 \
  --capture-preflight-only
```

Require `CAPTURE_PREFLIGHT_READY`; its adjacent JSON result distinguishes an
accepted frame from no ScreenCaptureKit output, no-image-buffer status frames,
layout rejection, pixel-format rejection, or an async stream error. It retains
no pixels. For the currently unresolved presentation-source differential,
`latency-target-window.swift --presentation-pattern --target-style borderless`
and `--target-style decorated` preserve the requested physical raster and the
nonactivating policy; the decorated form is a health comparison only and is
not a substitute matrix cell. Do not start a publisher control, observer, or
record a control/cell until the exact selected source style has emitted ready.

The normal coordinator path opens local SFU/browser/native windows and is an
operator-authorized live measurement only; do not substitute a screen capture,
focus change, raw-pixel dump, or an unowned cleanup command for its gates.

### Never-show-a-black-frame gates (#627)

CLAUDE.md's hard rule needs *pixels*, not events: "held the last frame" and
"went blank quietly" emit identical events, so an event-level assertion cannot
distinguish them. Both halves therefore sample rendered pixels across a forced
gap, and both run in BOTH directions — reproduce the failure first, then show
the fix holding — because a pass whose control never tripped proves only that
the gap failed to happen.

| Surface | Gate | Where it runs |
|---|---|---|
| Web viewer | `scripts/verify-no-black-frame.mjs` | `scripts/ci-local.sh` (headless Chromium needs nothing from the host) |
| Native compositor | `scripts/verify-no-black-frame-native.sh` | this tier — needs a window server **and** Screen Recording access. `ci-local.sh` only compiles `hold_last_frame_probe` so it cannot rot. |

```sh
scripts/verify-no-black-frame-native.sh
```

Measured on this host: baseline `mean_luma=255.0`; the pre-fix control took the
share's pixels off screen (`0.0` across the gap) while the fixed
`teardown_decision` path held them (`255.0`).

Three things about the native probe are load-bearing, each corrected after a
measurement contradicted the first attempt:

1. **It samples the screen REGION, not the window by id.** `screencapture
   -l<window>` returned `mean_luma=255` for a window that had *already been
   hidden* — it composites that window's own backing store regardless of
   whether it is on screen. Only a region capture answers "what would the user
   see here".
2. **A black backdrop window sits under the video window.** It makes the verdict
   independent of the host's wallpaper; without it a light desktop could read as
   "bright" and mask the failure.
3. **It validates itself before reporting anything.** A denied screen capture
   returns black, indistinguishable from the failure being measured, so the
   probe samples a known-bright window first and exits `3` as HARNESS INVALID
   rather than emitting a pass or a fail. An ad-hoc-signed `cargo build` example
   binary has no Screen Recording grant of its own, which is why capture is
   routed through the granted `screencapture(1)` instead of an in-process
   `CGWindowListCreateImage` (that path measured `mean_luma=0.0` throughout).

### #779 AX window-identity regression guard

```sh
scripts/verify-rc-window-identity.sh
```

`real_ax_window_identity_accepts_exact_window_and_refuses_same_app_sibling`
(`remote_control.rs`) is the only guard on the fix that made remote control work
again in 0.8.5. It reads REAL `AXWindow` elements and the real CG-backed window
registry, so it needs a window server, an Accessibility grant, and an
application serving two visible sibling windows -- and it is opt-in behind
`PETAL_RUN_REAL_AX_WINDOW_IDENTITY_TEST=1`. Nothing in the repo set that
variable, so between shipping the fix and 2026-08-10 the guard ran in exactly
zero automated invocations, and `ci-local.sh` runs `cargo test --lib` without
`--nocapture`, so even its `SKIP` line was swallowed. This script is what runs
it; `ci-local.sh` only compile-checks `scripts/probes/twowin.m`, the same
anti-rot rule the other probes get.

The fail-closed contract, in the same shape as
`scripts/verify-window-classification.sh`:

| Outcome | Exit | Meaning |
|---|---|---|
| `PASS` | 0 | the resolver told the fixture's two windows apart |
| `FAIL` | 1 | a real resolver defect -- the #779 class regressed |
| `HARNESS INVALID` | 3 | the run proved nothing: no Accessibility grant, `_AXUIElementGetWindow` missing, the fixture failed to open its windows, or the guard passed without ever naming the fixture's pid |

Two details are load-bearing and were both mistakes the first time round:

1. **Opted out is a choice; opted in but unusable is a failure.** Inside the
   test, every precondition below the env check now panics instead of
   returning. The runner translates the two purely-environmental panics (no AX
   grant, missing private symbol) back into exit 3, so a missing grant is never
   read as "the fix regressed".
2. **The guard must name this run's own fixture.** It walks every application
   with two sibling windows; passing on some unrelated app that happened to be
   open would mean the gate still passes with its fixture absent -- the exact
   "green regardless" shape it exists to remove. The runner greps the `PASS[...]`
   lines for the fixture's pid and calls the run INVALID if it is missing.

Accessibility is inherited from whatever launched the script (the terminal app
is the responsible process); grant that app Accessibility or the run exits 3.
The fixture is killed by PID on every exit path, including failures, and the
teardown verifies the kill rather than assuming it.

### Which republish event ordering actually wins (settled by measurement, #627)

Two code comments used to assert opposite orderings. Measured with
`share_lifecycle_probe` against a real (local) SFU, 10 consecutive runs:
`TrackSubscribed(new_sid)` arrived **84–135ms before** `TrackUnpublished(old_sid)`
in 10/10 runs, so `should_remove_window`'s sid guard does hold in practice.

That is a race the receiver does not control, though, and the losing side hid a
live share. `teardown_decision` no longer depends on it: the sender awaits its
new publish before unpublishing the old track
(`session/share.rs`'s `republish_*`/`unpublish_with_timeout`), so the SFU holds
the replacement publication whenever the unpublish exists at all. The receiver
asks `discover_window_publications` instead of trusting event order.

## Rosetta x86_64 peer tier (replaces "needs a second Mac" for code-path work)

`internal/docs/COURSE_CORRECTION.md` §3.2: there is no second Mac. Rosetta 2 gives us a
local **x86_64 code path** instead. Every Rust target is installed, so the same
crate builds and runs as a native x86_64 process on this Apple Silicon host.

### Build and run the x86_64 slice

`.cargo/config.toml` carries the CLT Swift link recipe for both targets, so the
only thing you must add is the deployment target:

```sh
cd apps/desktop/src-tauri

# tests
MACOSX_DEPLOYMENT_TARGET=13.0 cargo test --target x86_64-apple-darwin --lib

# an example probe (release; debug numbers are not comparable)
MACOSX_DEPLOYMENT_TARGET=13.0 \
  cargo build --release --target x86_64-apple-darwin --example camera_cadence_probe
./target/x86_64-apple-darwin/release/examples/camera_cadence_probe publish <room>
```

**`MACOSX_DEPLOYMENT_TARGET=13.0` is load-bearing.** Rust's default deployment
target for `x86_64-apple-darwin` predates macOS 10.14.4, so the linker
back-deploys the Swift runtime as `@rpath/libswiftCore.dylib`. The CLT rpath
only carries the concurrency shims, so the binary compiles and links fine and
then aborts at launch with `Library not loaded: @rpath/libswiftCore.dylib`.
With `13.0` the Swift libraries resolve to `/usr/lib/swift/...` absolute install
names and load from the dyld shared cache. The aarch64 build is unaffected
because its default target is already past that cutoff.

### Prove the run was actually translated, and actually equivalent

A green `--target x86_64-apple-darwin` run is worth nothing on its own: it can
be green because the Intel path is correct, or because the run never exercised
the Intel path, or because half the tests were `cfg`-ed away. §"cross-machine"
above already warns that *"an `x86_64` process alone is never Intel-hardware
evidence"*. Two cheap checks turn the result into evidence — both used to close
out #328's long-outstanding validation on 2026-08-09:

**1. Confirm the process is genuinely translated.** `sysctl.proc_translated`
reports `1` only inside a Rosetta-translated process, so read it from a probe
built for the same target:

```sh
cat > /tmp/archcheck.c <<'EOF'
#include <stdio.h>
#include <sys/sysctl.h>
int main(void){int t=0;size_t s=sizeof(t);
 if(sysctlbyname("sysctl.proc_translated",&t,&s,NULL,0)!=0){printf("UNAVAILABLE\n");return 0;}
 printf("proc_translated=%d\n",t); return 0;}
EOF
clang -arch x86_64 -o /tmp/archcheck_x86 /tmp/archcheck.c && /tmp/archcheck_x86   # -> 1
lipo -archs target/x86_64-apple-darwin/debug/deps/desktop_lib-<hash>             # -> x86_64
```

**2. Diff the test-name sets across the two arches.** A `#[cfg]` that quietly
drops the very tests you came to run is the failure mode this catches, and it
is invisible in a pass/fail line:

```sh
grep -oE "^test [a-zA-Z0-9_:]+" arm.log | sort -u > /tmp/arm.txt
grep -oE "^test [a-zA-Z0-9_:]+" x86.log | sort -u > /tmp/x86.txt
diff /tmp/arm.txt /tmp/x86.txt && echo "identical test sets"
```

Report the totals from both arches together; a bare "x86_64 passed" is not a
validation result.

### Two traps that silently invalidate a local two-peer media run

Both were paid for during #299 and cost several runs each. Neither announces
itself: both look exactly like "the feature is broken."

**1. A local SFU's RTC port range must sit OUTSIDE the OS ephemeral range.**
`sysctl net.inet.ip.portrange.first net.inet.ip.portrange.last` reports
`49152`–`65535` on this machine. A `livekit-server` config whose
`rtc.port_range_start/end` falls inside that window will intermittently pick a
port the *client* process in the same run has already taken as an ephemeral
source port. ICE still succeeds — you get `connectionType: udp`, a selected
candidate pair, a sub-20ms `connectTime` — and then DTLS never completes, so
the publisher's PeerConnection negotiation fails and **no track ever reaches
the SFU**. The symptom at the probe is `<no frames decoded>`, which reads as a
broken pipeline rather than a rig fault. Use a range well below 49152 (e.g.
`20000`–`21000`).

**2. A "is the app running?" guard does not protect a timing measurement.**
Gating a run on `pgrep -f target/debug/desktop` catches single-instance and
media-device contention, but a **compile storm has no Petal process and is
invisible to it**. Measured during #299 on this machine: decoded fps for the
same code was `28.0` at load average `2.2` and `9.1` at load `7.3` — a swing
larger than most effects worth measuring. Gate on `uptime`'s 1-minute load as
well as process presence, check it *after* the run as well as before (load
rises mid-run), and **re-take** a failing iteration rather than annotating it.

### Two-peer cross-architecture camera cadence (#549)

```sh
# 1. local LiveKit, and apps/desktop/.env pointed at it
livekit-server --config <your-dev-config>

# 2. receiver
cargo run --release --example camera_cadence_probe -- subscribe petal-549 40

# 3. sender, aarch64 baseline
cargo run --release --example camera_cadence_probe -- publish petal-549 30

# 4. sender, x86_64 code path
./target/x86_64-apple-darwin/release/examples/camera_cadence_probe publish petal-549 30
```

The probe's source is a pre-rendered synthetic NV12 ring, so the input bytes are
identical in both runs and no camera hardware or camera TCC grant is involved:
any difference between the two reports is the pipeline, not the webcam. It
reports `convert_ms` (NV12→I420), `capture_frame_ms` (handoff into libwebrtc),
`push_fps`, and `late_ticks` on the sender, and decoded fps plus inter-frame gap
percentiles on the receiver.

### What this tier does and does not prove

Rosetta executes the **x86_64 code path** — SIMD kernel selection (`libyuv`
NEON vs SSE2/AVX2), codec and pixel-format branches, pointer widths. A defect
that reproduces here is real and a fix for it is validated.

It runs on Apple Silicon hardware. It does **not** exercise genuine Intel
silicon, an Intel GPU, or Intel VideoToolbox hardware encoders, and Rosetta's
translation overhead makes its CPU timings pessimistic relative to native
x86_64. A defect that does *not* reproduce here is weaker evidence: say so
rather than closing the issue as fixed. `scripts/cross-machine-rc-suite.sh`
still rejects translated execution for release evidence, and that stays true.

## Environment Variables

| Variable | Used by | Purpose |
|---|---|---|
| `LIVEKIT_URL` | backend, web-harness token middleware, Rust probes/harness debug token minting, desktop debug fallback | LiveKit signaling URL. Local default is commonly `ws://localhost:7880`. |
| `LIVEKIT_API_KEY` | backend, web-harness token middleware, Rust probes/harness debug token minting, desktop debug fallback | LiveKit API key used server-side or in dev probes to mint JWTs. |
| `LIVEKIT_API_SECRET` | backend, web-harness token middleware, Rust probes/harness debug token minting, desktop debug fallback | LiveKit API secret. Must not be exposed to shipped client JS or release app joins. |
| `PETAL_ADMIN_TOKEN` | `backend/lib/handlers.ts` | Bearer token required to authorize the admin room-control endpoint (kick/close); the endpoint refuses all requests with a 503 if unset. |
| `PETAL_ALLOWED_ORIGINS` | `backend/lib/http.ts` | Comma-separated list overriding the backend's default CORS-allowed origins (`app.petal.live`, `meet.petal.live`). |
| `PETAL_BACKEND_URL` | desktop Rust token client, `apps/desktop/.env.example` | Base URL for the backend token/rooms API. Trimmed of trailing slashes. Debug desktop builds can join via local LiveKit fallback when unset; release builds require it. |
| `VITE_PETAL_BACKEND_URL` | `web-harness/src/main.ts` | Browser-harness backend base URL for deployed testing. |
| `PETAL_AUTOTEST_ROOM` | desktop autotest | Enables startup join into a room. |
| `PETAL_AUTOTEST_IDENTITY` | desktop autotest | LiveKit identity for autotest joins; defaults to `p-native-autotest` (must match the backend's `GENERATED_PARTICIPANT_ID` shape when joining against a real backend -- the old `native-autotest` default never did). |
| `PETAL_AUTOTEST_NAME` | desktop autotest | Display name for autotest joins; defaults to the identity. |
| `PETAL_AUTOTEST_SHARE` | desktop autotest | `auto`, `owner:<AppName>`, or a numeric `CGWindowID` to share after joining. Omit for receiver-only. |
| `PETAL_AUTOTEST_TOGGLE_SECS` | desktop autotest | Repeatedly stop/start the selected share after this interval. |
| `PETAL_AUTOTEST_TOGGLE_CYCLES` | desktop autotest | Number of stop/start cycles; defaults to the code's fallback when unset. |
| `PETAL_AUTOTEST_SOCK` | desktop autotest and scenario scripts | Unix socket path for newline-delimited JSON commands. |
| `PETAL_AUTOTEST_SHARE_WITH_BORDER` | `apps/desktop/src-tauri/src/autotest.rs` | Enables the bordered hover-tab share path for the autotest's initial share instead of the plain toggle-share path, exercising the exact hover-tab lifecycle. |
| `PETAL_AUTOTEST_SHARE_COLOR` | `apps/desktop/src-tauri/src/hover_tab.rs` | Overrides the hover-tab/share-border color used when no explicit color is supplied to a share. |
| `PETAL_BACKEND_URL` | `apps/desktop/scripts/cockpit.mjs`, `test_cockpit` engine | Prod backend to mint tokens against; default `https://app.petal.live`. |
| `PETAL_HARNESS_URL` | `apps/desktop/scripts/cockpit.mjs`, `test_cockpit` engine | web-harness origin the headless Chrome peer navigates to; default `https://meet.petal.live`. |
| `PETAL_CHROME_BIN` | `apps/desktop/scripts/cockpit.mjs` | Path to the branded Google Chrome binary to launch headless; default `/Applications/Google Chrome.app/Contents/MacOS/Google Chrome`. |
| `PETAL_COCKPIT_ARTIFACT_RETENTION_DAYS` | `test_cockpit` engine | Max age for pruning video/audio artifacts referenced by `run.jsonl`; default `14`. Structured `run.jsonl` and `scorecard.json` are never pruned. |
| `PETAL_COCKPIT_ARTIFACT_RETENTION_RUNS` | `test_cockpit` engine | Minimum recent runs per scenario whose video/audio artifacts are retained even if older than the age cutoff; default `20`. |
| `PETAL_COCKPIT_NATIVE_PEER_SOCKET` | `test_cockpit/mod.rs` | Per-run Unix socket path the parent cockpit process injects into the spawned test-peer binary for command handoff during the SHARE-N2N scenario. |
| `PETAL_COCKPIT_NATIVE_PEER_TOKEN` | `test_cockpit/mod.rs` | Per-run authentication token for that native-peer Unix socket, generated and injected by the parent. |
| `PETAL_COCKPIT_NATIVE_PEER_ROOM` | `test_cockpit/mod.rs` | Real access-code/room name the spawned test-peer process must join to meet the parent sharer (the parent's real access code, not the bare internal room credential -- see #421/#430). |
| `PETAL_COCKPIT_NATIVE_PEER_OWNER` | `test_cockpit/mod.rs` | Expected LiveKit identity of the sharer the test-peer receiver must bind its remote compositor to. |
| `PETAL_COCKPIT_NATIVE_PEER_WINDOW` | `test_cockpit/mod.rs` | Numeric `CGWindowID` of the shared source window the test-peer receiver expects to see enqueued. |
| `PETAL_COCKPIT_NATIVE_PEER_IDENTITY` | `test_cockpit/mod.rs` | LiveKit identity the spawned test-peer process itself joins the room as (must match the backend's `GENERATED_PARTICIPANT_ID` shape). |
| `PETAL_COCKPIT_FRONTEND_PROVENANCE` | `apps/desktop/scripts/build-cockpit-primary.sh`, `test_cockpit/mod.rs` | Self-computed git-commit + asset-checksum fingerprint the build script exports, asserts, and bakes into the QA binary via `env!`; an internal build-provenance value, not an external toggle -- a bare `cargo build` produces an "unverified" provenance that fails scenarios needing test-pattern assets. |
| `PETAL_COCKPIT_SETUP_CONFIRMED` | `apps/desktop/scripts/cockpit-setup.sh` | Operator-acknowledgement gate (`1`) confirming Screen Recording/Accessibility were granted before this lightweight helper writes its non-TCC setup marker. |
| `PETAL_COCKPIT_XCODE_DEVELOPER_DIR` | `apps/desktop/scripts/cockpit-runtime-policy.sh` | Full-Xcode `Developer` directory used as the Swift static-archive link source when building QA cockpit binaries; default `/Applications/Xcode.app/Contents/Developer`. |
| `PETAL_TEST_PEER_IDENTIFIER` | `apps/desktop/scripts/build-test-peer.sh` | Overrides the test-peer binary's fixed Tauri bundle identifier; default `com.petal.app.testpeer`. |
| `PETAL_TEST_PEER_FEATURES` | `apps/desktop/scripts/build-test-peer.sh` | Extra comma-separated Cargo features appended to `cockpit-privileged` when building the test-peer binary. |
| `PETAL_PEER_TEST_MODE` | `apps/desktop/scripts/test-build-test-peer-runtime.sh` (fake `otool` fixture it generates) | Self-test-only switch making the fixture's fake `otool` report a forbidden CommandLineTools rpath, to regression-test the #315 runtime-policy failure path itself. |
| `PETAL_TEST_CASE` | `test_cockpit/mod.rs`, `apps/desktop/scripts/cockpit.mjs` | Default cockpit scenario/test-case selector used when no `--test-case` CLI argument is given; defaults to `quick`. |
| `PETAL_DISABLE_AUDIO` | desktop session | Skips mic publish and speaker playout for video-only test runs. **`0`/`false`/`no`/`off` mean ENABLED** — it used to treat any non-empty value as "disable", so `=0` silently skipped the mic (#812). |
| `PETAL_ACCESSORY_UI` | app activation policy | `1` puts the app in Accessory policy at startup: no Dock tile, no Cmd-Tab entry, self-activation becomes a no-op (the main window is shown, never focused). Set by every cockpit native-peer spawn, `rc-live-suite.sh`, and the `verify-*` audio scripts (#823). Do not set it for a scenario that asserts on real activation semantics. |
| `PETAL_AUDIO_SYNTH_TONE` | desktop mic publish | Substitutes a deterministic 440Hz tone for microphone INPUT, leaving the entire publish path real. Exists because an agent machine's mic records a silent room, so a green audio run would otherwise prove nothing. |
| `PETAL_AUDIO_PUBLISH_UNMUTED` | desktop mic publish | Publishes the mic unmuted and refuses the session's join-time mute. **Only honored together with `PETAL_AUDIO_SYNTH_TONE=1`** — un-gated it would join a real microphone hot while the UI still read muted. Petal joins muted, so rigs with no UI to click unmute need this or they measure correct-but-useless silence. |
| `PETAL_CAMERA_SYNTH_SOURCE` | desktop camera publish (`transport::camera::open_camera`) | Substitutes a deterministic NV12 test pattern — a bright bar sweeping a mid-grey field — for camera INPUT, leaving the entire publish path real. Exists because an agent machine may have no camera at all, and a real camera in a dark room delivers frames a receiver cannot tell apart from a broken pipeline. Used by cockpit `CAM-N2W` (journey CAM-05, #815). |
| `PETAL_CAMERA_SYNTH_FREEZE` | desktop camera publish | Holds the synthetic pattern on one frame, so the picture stops changing while frames keep being delivered. This is `CAM-N2W`'s **mutation lever**: with it set, a live run must go TEST-FAIL, which is how you prove the oracle can fail at all. **Only honored together with `PETAL_CAMERA_SYNTH_SOURCE=1`** — un-gated it would be a variable that alters what a REAL camera publishes. |
| `PETAL_DISABLE_DIRTY_RECT_SKIP` | desktop share pump | Set to `1` to disable dirty-rect-clean frame skipping during live debugging; remote-control cadence remains unchanged. |
| `PETAL_DISABLE_SNAPSHOT_PULL` | `apps/desktop/src-tauri/src/session/share.rs` | Kill switch (`1`) disabling the #183 `SCScreenshotManager` snapshot-pull fallback in the share frame pump (also self-disables on hard API errors, e.g. macOS 13). **Debug builds only** — compiled out of release. |
| `PETAL_DISABLE_RECONNECT_SHARE_REPAIR` | `apps/desktop/src-tauri/src/resilience.rs` | Emergency kill switch (`1`) for Petal's delayed post-reconnect share-republish repair (#298), leaving only LiveKit's own reconnect/resume behavior active. **Debug builds only** — compiled out of release. |
| `PETAL_DISABLE_NATIVE_PUBLISH` | `apps/desktop/src-tauri/src/transport/publisher.rs` | Disables the native CVPixelBuffer publish path, forcing the NV12→I420 conversion fallback from the first frame onward. **Debug builds only** — compiled out of release. |
| `PETAL_FORCE_CODEC` | `apps/desktop/src-tauri/src/transport/publisher.rs` | Forces the window-share video codec to `h264`, `av1`, or `h265`/`hevc` instead of the default H.264, for the #184/#188 encoder-readback spike. |
| `PETAL_TEST_UNPUBLISH_DELAY_MS` | `apps/desktop/src-tauri/src/transport/publisher.rs` | Debug/test-only delay (ms) injected into `PublishedTrack::unpublish` for cockpit-driven network-tail fault injection (#420); compiled only for `test`/`debug_assertions` builds. |
| `PETAL_TEST_UNPUBLISH_FAIL` | `apps/desktop/src-tauri/src/transport/publisher.rs` | Debug/test-only switch making `PublishedTrack::unpublish` return an injected failure instead of unpublishing, for the same #420 fault-injection path. |
| `PETAL_SENTRY_DSN` | `apps/desktop/src-tauri/src/logging.rs` | Local-testing runtime override for the Sentry crash-reporting DSN, checked before falling back to the value baked in at compile time via `option_env!`. |
| `PETAL_POSTHOG_KEY` | `apps/desktop/src-tauri/src/analytics.rs` | Local-testing runtime override for the PostHog project token (`phc_…`), checked before falling back to the compile-time bake. Absent (the default for every `cargo test` / `tauri dev` run) means product events are a no-op. Never commit the token. |
| `PETAL_POSTHOG_HOST` | `apps/desktop/src-tauri/src/analytics.rs` | Optional PostHog ingest host override; default `https://us.i.posthog.com`. |
| `VITE_PETAL_POSTHOG_KEY` | `web-harness/src/analytics.ts` | Browser-client bake of the same Petal PostHog project token. Set on the web-harness Vercel project for `meet.petal.live`; absent in local/CI (`--build-only`) so product events are a no-op. Never commit the token. Do not add `posthog-js`. |
| `VITE_PETAL_POSTHOG_HOST` | `web-harness/src/analytics.ts` | Optional PostHog ingest host override for the browser client; default `https://us.i.posthog.com`. |
| `PETAL_CRISP_STILL_SPIKE` | desktop share pump (`crisp_still.rs`) | Set to `1` to enable the #384 crisp-mode still-image spike (encodes + publishes a lossless WebP still on static windows). **Default OFF** — unlike this table's other `PETAL_DISABLE_*` switches, this one opts IN: there is no receiver-side blit, so enabling it burns CPU/bandwidth for zero visible benefit. Spike-only, not for normal use. |
| `PETAL_AI_CONTROL` | `apps/desktop/src-tauri/src/ai_chat/control_gate.rs` | Master switch (`1`) arming AI chat's permission-gated control of the shared window (#658). **Default OFF, and it opts IN.** With it unset the window-control tools are never declared to the model, so there is nothing to call, nothing to approve and no execution path to reach — verified by `ai_chat::session` tests. It stays off by default because the end-to-end path has NOT been exercised against a live model (the Gemini project is out of credits), and unverified code that clicks and types in a user's real applications must not be live on the strength of unit tests alone. Arming it does not bypass anything: every action still needs a human approval and passes the full fail-closed re-check at the moment it runs. |
| `PETAL_COMPOSITOR_NO_CHROME` | compositor debug | Skips compositor header and telepointer child windows. |
| `PETAL_COMPOSITOR_DEBUG_BG` | compositor debug | Paints the video layer background for occlusion debugging. |
| `PETAL_REMOTE_CONTROL_TARGET_IDENTITY` | `apps/desktop/scripts/remote-control-scenario.mjs` | Expected native target identity; default `native-autotest`. |
| `PETAL_REMOTE_CONTROL_DIRECT_SCROLL` | `apps/desktop/src-tauri/src/remote_control.rs` | Opt-in switch (`1`) routing remote-control scroll events through fire-and-forget SkyLight event posting instead of the default path. **Measured ineffective — stays OFF, see below (#446).** |
| `PETAL_REMOTE_CONTROL_DIRECT_DRAG` | `apps/desktop/src-tauri/src/remote_control.rs` | Opt-in switch (`1`) letting a single direct-injection route own an entire remote-control drag gesture (down/move/up) instead of the default AX replay path. **Measured ineffective — stays OFF, see below (#446).** |
| `PETAL_REMOTE_CONTROL_DIRECT_CLICK` | `apps/desktop/src-tauri/src/remote_control.rs` | Opt-in switch (`1`) sending left-button remote-control clicks via direct SkyLight event posting instead of the default semantic-click replay (#369). **Measured ineffective — stays OFF, see below (#446).** |
| `PETAL_REMOTE_CONTROL_CDP_JSON` | remote-control scenario | Chrome DevTools `/json` endpoint; default `http://127.0.0.1:9222/json`. |
| `PETAL_WEB_HARNESS_URL_MATCH` | remote-control scenario | URL substring used to find the web-harness Chrome tab; cockpit-drive derives the host from `PETAL_HARNESS_URL` (default `meet.petal.live`), while standalone runs retain their script default. |
| `PETAL_REMOTE_CONTROL_ACQUIRE_TIMEOUT_MS` | remote-control loopback/scenario | Max time for controller-side request/first-input publish metrics; default `7000`. |
| `PETAL_REMOTE_CONTROL_STATUS_TIMEOUT_MS` | remote-control loopback/scenario | Max time for native-host active status to return over the data channel; defaults to acquire timeout. |
| `PETAL_REMOTE_CONTROL_INPUT_BUDGET_MS` | remote-control loopback/scenario | Max wall-clock time for request + immediate first click + first text to land in TextEdit; default `500`. |
| `PETAL_REMOTE_CONTROL_SHARE_READY_TIMEOUT_MS` | `apps/desktop/scripts/remote-control-scenario.mjs` | Max time to wait for the shared window to become ready before a scenario proceeds; default `8000`. |
| `PETAL_REMOTE_CONTROL_CASE_SETTLE_MS` | `apps/desktop/scripts/remote-control-scenario.mjs` | Settle delay after each scenario case before the next one runs; default `500`. |
| `PETAL_REMOTE_CONTROL_RECONNECT_MODE` | `apps/desktop/scripts/remote-control-scenario.mjs` | Reconnect mode (`resume` by default) requested via the `reconnect` command in the lifecycle/reconnect scenario case. |
| `PETAL_REMOTE_CONTROL_PHOTON_SAMPLES` | `apps/desktop/scripts/remote-control-scenario.mjs` | Number of press-to-photon latency samples collected per input in `--press-to-photon` mode; default `20`. |
| `PETAL_REMOTE_CONTROL_PHOTON_WARMUP_SAMPLES` | `apps/desktop/scripts/remote-control-scenario.mjs` | Number of warm-up photon samples discarded before recording per-input latency stats; default `2`. |
| `PETAL_REMOTE_CONTROL_PHOTON_TIMEOUT_MS` | `apps/desktop/scripts/remote-control-scenario.mjs` | Max time to wait for a single photon-sentinel sample event before treating it as missed; default `2000`. |
| `PETAL_REMOTE_CONTROL_PHOTON_P95_BUDGET_MS` | `apps/desktop/scripts/remote-control-scenario.mjs` | p95 press-to-photon latency budget (ms) a scenario run must stay under to pass; default `250`. |
| `PETAL_RC_SENTINEL_EVENT_LOG` | `apps/desktop/scripts/remote-control-photon-sentinel.swift` | Path to the JSONL event log the photon-sentinel helper app writes press/paint timing events to; default `/tmp/petal-rc-sentinel-events.jsonl`. |
| `PETAL_REMOTE_HOST` | `scripts/cross-machine-rc-suite.sh` | SSH target (`user@host` or `~/.ssh/config` alias) for a second real Mac to run the sharer role on. Required to use that script at all. |
| `PETAL_REMOTE_APP_DIR` | `scripts/cross-machine-rc-suite.sh` | Where the Developer-ID-signed test bundle is deployed on the remote Mac; default `/tmp/petal-cross-machine-test`. |
| `PETAL_REMOTE_KEEP_APP` | `scripts/cross-machine-rc-suite.sh` | Set to `1` to leave the deployed remote `Petal.app` in place during cleanup; defaults to removing it. |
| `PETAL_REMOTE_OSASCRIPT_HOST` | remote-control scenario | When set, routes `osascript()`/`osascriptRaw()` through `ssh $HOST osascript` instead of running locally; set automatically by `cross-machine-rc-suite.sh`. |
| `PETAL_REMOTE_COMMAND_HOST` | `scripts/cross-machine-rc-suite.sh` (generated PATH-shim wrappers) | SSH host that the suite's generated `open`/`pkill`/`pbcopy`/`pbpaste`/`defaults`/`sample`/`screencapture` PATH shims forward local commands to during a cross-machine remote-control run. |
| `PETAL_RUN_REAL_AX_WINDOW_IDENTITY_TEST` | `remote_control.rs`'s `real_ax_window_identity_accepts_exact_window_and_refuses_same_app_sibling` | Set to `1` to run the #779 AX window-identity regression guard against real `AXWindow` elements. Unset, the guard returns early (the deliberate opt-out); set, every remaining precondition is a panic, not a skip. Set for you by `scripts/verify-rc-window-identity.sh`, which is the supported way to run it. |
| `PROBE_SPOT_X` / `PROBE_SPOT_Y` | `scripts/probes/onewin.m`, `scripts/probes/twowin.m` and their runners | Screen point (top-left origin) the probe windows are placed around, so a run can aim at an empty region on any display topology; defaults `2300`/`800` (the 2560-wide dev rig). |
| `TWOWIN_SECONDS` | `scripts/probes/twowin.m` | Backstop lifetime for the two-window AX fixture; default `300`. `scripts/verify-rc-window-identity.sh` kills the fixture by PID as soon as the guard has run, so this only bounds an orphan -- never rely on it for teardown. |
| `RUST_LOG` | desktop logging and Rust examples | Overrides the default Rust log level. |
| `BLOB_READ_WRITE_TOKEN` | backend Blob helpers, `scripts/publish-blob.mjs` | Vercel Blob token used to list/read release artifacts and publish them from scripts. |
| `TAG` | `scripts/publish-blob.mjs` | Release tag used as a notes fallback; defaults to `v<package version>`. |
| `PETAL_INCLUDE_DEV_ROUTES` | `apps/desktop/svelte.config.js` | Set to `1` to keep `/dev/*` routes during `npm run build`. |
| `PETAL_BUNDLE_ID` | `scripts/release-smoke.sh` | macOS bundle identifier the release smoke test expects the signed app to have; default `com.petal.app`. |
| `PETAL_RELEASE_TEAM_ID` | `scripts/release-smoke.sh` | Apple Developer Team ID the release smoke test verifies the signed app/DMG against; default `X83RP84J8Z`. |
| `PETAL_RELEASE_BACKEND_URL` | `scripts/release-smoke.sh` | Backend URL referenced by the release smoke checklist for the clean-TCC live pass; default `https://app.petal.live`. |
| `PETAL_RELEASE_APP` | `scripts/release-smoke.sh` | Path to the signed `.app` bundle to verify; required for the static-artifact assertions. |
| `PETAL_RELEASE_DMG` | `scripts/release-smoke.sh` | Path to the release DMG to optionally verify stapling/notarization on. |
| `PETAL_RELEASE_LOG` | `scripts/release-smoke.sh` | Path to `petal.log` that `--assert-log` scans for the expected permission/share/remote-control markers; default `~/Library/Logs/Petal/petal.log`. |
| `PETAL_WEB_HARNESS_URL` | `scripts/verify-web-harness-live.sh` | Base URL of the deployed web-harness site the post-deploy smoke check hits for landed-feature markers; default `https://meet.petal.live`. Distinct from `PETAL_WEB_HARNESS_URL_MATCH` above. |
| `PETAL_NET_IMPAIR_PFCTL` | `scripts/net-impair.sh` | Overrides the path to the `pfctl` binary used to install CHAOS-NET packet-filter rules; default `/sbin/pfctl`. |
| `PETAL_NET_IMPAIR_DNCTL` | `scripts/net-impair.sh` | Overrides the path to the `dnctl` binary used to configure dummynet queues for impairment profiles; default `/usr/sbin/dnctl`. |
| `PETAL_NET_IMPAIR_DSCACHEUTIL` | `scripts/net-impair.sh` | Overrides the path to `dscacheutil`, used to resolve/flush the LiveKit host's DNS entries; default `/usr/bin/dscacheutil`. |
| `PETAL_NET_IMPAIR_HOST_CMD` | `scripts/net-impair.sh` | Overrides the path to the `host` binary used to resolve the LiveKit host to IPs; default `/usr/bin/host`. |
| `PETAL_NET_IMPAIR_DRY_RUN` | `scripts/net-impair.sh` | Set to `1` to print the pfctl/dnctl commands the script would run without actually applying network impairment. |
| `PETAL_NET_IMPAIR_STATE_DIR` | `scripts/net-impair.sh` | Overrides the directory where the script persists its pf token/profile/host/IP state between `on`/`off`/`status` invocations; default `/var/run/petal-net-impair`. |
| `PETAL_LIVEKIT_HOST` | `scripts/net-impair.sh` | Explicit LiveKit host to scope impairment to, taking priority over deriving the host from `LIVEKIT_URL`. |
| `TAURI_DEV_HOST` | `apps/desktop/vite.config.js` | Optional Vite dev host override used by Tauri dev. |
| `DEVELOPER_DIR` | build scripts/toolchain | Points builds at Command Line Tools or Xcode. `apps/desktop/scripts/dev.sh` defaults it to CLT. |
| `RUSTFLAGS` | Rust builds | Used in documented CLT/full-Xcode recipes and by cargo config; not needed for docs-only work. |
| `DYLD_LIBRARY_PATH` | running raw debug binaries | Runtime Swift dylib path for some unbundled binaries. |
| `DYLD_FALLBACK_LIBRARY_PATH` | `cargo test --lib`, `scripts/ci-local.sh` | Runtime Swift dylib fallback used by the Rust test harness on this machine. |
| `SCREENCAPTUREKIT_ALLOW_STUBBED_BUILD` | vendored `screencapturekit` build script | Opts into a stubbed build when SDK detection fails. Do not use for normal Petal validation. |

## #446 — the direct (SkyLight) pointer routes are measured ineffective

`PETAL_REMOTE_CONTROL_DIRECT_CLICK` / `_DRAG` / `_SCROLL` exist so remote-control
pointer input can bypass the Accessibility path via `SLEventPostToPid`. They were
left opt-in "pending live validation". **That validation ran on 2026-07-27 and
they failed it. Do not default them on.**

Setup — one Mac, macOS 26.5.2 (25F84) arm64, `web-harness` browser controller →
native host (`npm run dev:clean`, Screen Recording + Accessibility both GRANTED)
→ a shared window belonging to a small AppKit app that appends every
`NSWindow.sendEvent:` it receives to a log file. Both controller flavors were
driven: `api.click(...)` (web-harness v2 semantic `Click`) and
`api.pointer({action:'down'|'up'})` / `api.drag(...)` (the raw Legacy
Down/Move/Up dispatch a desktop controller sends).

| Input | `DIRECT_*` unset (default) | `DIRECT_*=1` |
|---|---|---|
| v2 semantic left click | 0 NSEvents | 0 NSEvents, host logs `route=direct` |
| legacy raw Down/Up click | 0 NSEvents | 0 NSEvents, host logs `mode=SlDrag outcome=Handled` |
| legacy Down/Move/Up drag | 0 NSEvents, host warns `pointer or wheel injection exhausted AX/SkyLight routes` | 0 NSEvents, **no warning at all** |
| wheel | 0 NSEvents | 0 NSEvents |
| keyboard (control) | delivered | delivered |
| paste, Cmd+V (control) | delivered | delivered |

The control rows are what make this conclusive: the same run, same window, same
grant — keyboard and paste landed, pointer input never did. Delivery is
identical with the routes on and off; the only change is that turning them on
**suppresses the honest failure**, because `SLEventPostToPid` is bound as a
void-returning function whose result is never inspected. `route=direct` and
`mode=SlDrag outcome=Handled` mean "posted", never "delivered".

A standalone Swift probe (`dlopen` SkyLight → `SLEventPostToPid` with a
`CGEventCreateMouseEvent` at in-window global coordinates) reproduced the same
nothing against the same target, while a real HID-path keystroke into that
target logged `keyDown`/`keyUp` immediately — so the observation channel was
sound and the failure is in SkyLight posting itself, not in the harness.

`remote_control.rs`'s `direct_pointer_routes_stay_opt_in_after_the_446_live_pass`
test pins this. Anyone flipping a default needs a **new** measurement showing a
real delivered NSEvent in a real target app.
