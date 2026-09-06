# Releasing Petal

Two ways to cut a signed, notarized release. **Local is the default right now**
(no CI cost); the cloud pipeline is fully configured and kept as the
"push-button, universal" path for when it's worth the runner spend.

| | Local (default) | Cloud (CI) |
|---|---|---|
| Arch | **arm64** (Apple Silicon) | **universal** (arm64 + x86_64) |
| Runs on | your Mac | GitHub Actions (`macos-26`, ~10× billing) |
| Signs + notarizes + staples | yes (you run the steps) | yes (automatic) |
| Feeds auto-update | optional (see below) | yes (uploads `latest.json` to Vercel Blob) |
| Trigger | run the commands below | push a `vX.Y.Z` tag (publishes), or **Actions → Release → Run workflow** for a dry run / a self-hosted runner |
| Proven | ✅ (arm64 DMG verified notarized/accepted) | ✅ dry runs through notarize + staple + smoke on `macos-26` (2026-09-03); first *published* cloud release still pending |

Both produce a DMG that opens on a stock Mac with no Gatekeeper warning.

---

## Before you start: run the notarization preflight

```
scripts/preflight-notarization.sh
```

**Run it before the build, and again right before notarizing if the build took
a while.** It exits non-zero and tells you exactly what to do.

**The recurring failure it exists to stop.** The login keychain auto-locks on
an inactivity timer (currently **7200s / 2h** on this Mac). A universal build
plus notarization routinely straddles that window, so the keychain relocks
*mid-release*. `notarytool` then fails with **"No Keychain password item"**,
which reads like broken or missing credentials. It is not. It is the timer.

Every instinctive response to that message is wrong and none of them fix it:
re-running `notarytool store-credentials`, running `gh auth login`, switching
git to SSH. The same relock also makes `gh` report "token is invalid" and
`git push` report "could not read Username" — three tools, three misleading
errors, one cause.

**To stop it happening at all**, pick one. Both are yours to run — one changes
a security setting, the other adds a credential, so tooling must not do either
unprompted:

1. Stop the login keychain auto-locking on idle:
   ```
   security set-keychain-settings ~/Library/Keychains/login.keychain-db
   ```
   Re-arm later with
   `security set-keychain-settings -u -t 7200 ~/Library/Keychains/login.keychain-db`.

2. **Preferred, and the chosen direction — take the keychain out of this path
   entirely.** An App Store Connect API key is read from a *file*, so
   notarization stops depending on keychain lock state at all. The tooling
   already supports this; it activates the moment the key exists.

   **One-time setup:**
   a. Generate the key at appstoreconnect.apple.com → Users and Access →
      Integrations → App Store Connect API. Role **Developer** is sufficient
      for notarization. You can download the `.p8` exactly once.
   b. Save it and lock it down:
      ```
      mkdir -p ~/.appstoreconnect/private_keys
      mv ~/Downloads/AuthKey_<KEYID>.p8 ~/.appstoreconnect/private_keys/
      chmod 600 ~/.appstoreconnect/private_keys/AuthKey_<KEYID>.p8
      ```
   c. Record the two identifiers (Key ID from the row, Issuer ID from the top
      of that page) in `~/.claude/secrets/petal-notary-api.env`:
      ```
      NOTARY_API_KEY_ID=<KEYID>
      NOTARY_API_ISSUER=<ISSUER-UUID>
      NOTARY_API_KEY_PATH=$HOME/.appstoreconnect/private_keys/AuthKey_<KEYID>.p8
      ```
   d. Confirm: `scripts/preflight-notarization.sh` should report
      `notarization auth: apikey`.

   From then on, use **`scripts/notarize.sh`** wherever you would have run
   `xcrun notarytool` — it supplies auth automatically, preferring the API key
   and falling back to the keychain profile if the config is absent:
   ```
   scripts/notarize.sh submit "$DMG" --wait
   scripts/notarize.sh history
   ```
   It refuses to proceed if the key file is missing, is not mode 600/400, or
   the config still holds an un-substituted placeholder.

   **Do not** write `xcrun notarytool ... $(scripts/notary-auth.sh)`. That
   idiom depends on the shell word-splitting an unquoted expansion: bash does,
   **zsh does not** — and this Mac's interactive shell is zsh, so it fails with
   `Unknown option '--key ... --key-id ...'`. The wrapper passes argv directly
   and has no such dependency.

   **Note the remaining keychain dependency:** `codesign` still needs the login
   keychain for the Developer ID certificate. The API key removes the relock
   risk from *notarization*, which is the long tail of a release, but signing
   happens early and fast, so the exposure shrinks to minutes.

   This is also exactly what a CI runner needs, so it removes the local/cloud
   divergence at the same time.

## One-time prerequisites (already set up on the build Mac)

- **Full Xcode** installed + licensed (`xcodebuild -version` works).
- **A "Developer ID Application" certificate** for your own Apple Developer
  account, in the login keychain (`security find-identity -v -p codesigning`).
- **A notary keychain profile.** If it's missing, create one (needs an
  app-specific password from appleid.apple.com):
  ```
  xcrun notarytool store-credentials "$NOTARY_PROFILE" \
    --apple-id "$APPLE_ID" --team-id "$APPLE_TEAM_ID" --password <app-specific-pw>
  ```

  Set these in your shell (or a local, untracked `.envrc`) before running any
  command below. They are per-maintainer values and deliberately not committed:
  ```
  export APPLE_ID="you@example.com"
  export APPLE_TEAM_ID="YOURTEAMID"
  export NOTARY_PROFILE="your-notary-profile"
  export APPLE_SIGNING_IDENTITY="Developer ID Application: Your Org (YOURTEAMID)"
  ```

  Petal's own official release values live in the private operations runbook,
  not here.
- **Updater signing key** at `~/.tauri/petal-updater.key` (minisign, no
  password). Its public key is baked into
  `apps/desktop/src-tauri/tauri.release.conf.json` (the release-only overlay;
  the committed `tauri.conf.json` deliberately ships an EMPTY updater so a
  build from a plain clone never phones home) — don't rotate it without
  updating that pubkey, or existing installs can't verify updates. Every
  release recipe below passes `--config src-tauri/tauri.release.conf.json`;
  a `tauri build` without it produces a binary with auto-update disabled.

For **cloud**, the repo secrets must be set in GitHub → Settings → Secrets →
Actions; see the release.yml header for the list. Verify them with a
`publish=false` dry run of the Release workflow (see "Cloud release").

---

## Bump the version first — EVERY build (both paths)

**Rule: bump the version on every single build — increment the PATCH (last)
number — and never rebuild the same version number.** So `0.2.0` → `0.2.1` →
`0.2.2` … Keep the minor/major digits reserved for meaningful releases so the
middle number doesn't climb fast. A fresh version per build keeps DMG filenames,
notarization records, and the auto-updater (`latest.json` compares versions)
unambiguous.

Use the bump tool — it writes every mirror in one go:

```
node scripts/bump-version.mjs <new-version>   # e.g. 0.9.7
node scripts/version-lockstep.mjs             # self-check: all nine fields agree
```

The **nine lockstep fields** across seven files (`scripts/version-lockstep.mjs`
is the authority; `ci-local.sh` runs it):
- `apps/desktop/src-tauri/tauri.conf.json`  (`"version"` — drives the DMG name + updater manifest)
- `apps/desktop/src-tauri/Cargo.toml`       (`version = "…"`)
- `apps/desktop/src-tauri/Cargo.lock`       (the `name = "desktop"` package entry)
- `apps/desktop/package.json`               (`"version"`)
- `apps/desktop/package-lock.json`           (top-level `version` **and** `packages[""].version`)
- `web-harness/package.json`                 (`"version"` — the isolated Vercel build mirror)
- `web-harness/package-lock.json`            (top-level `version` **and** `packages[""].version`)

Editing these by hand and missing `Cargo.lock` or a lockfile's `packages[""]`
entry fails the gate, which is why the tool exists.

The release workflow checks these mirrors against the tag and fails before a
desktop artifact is built if any value drifts. The web harness is built from
its own Vercel project root, so it cannot rely on reading the sibling desktop
package at deploy time; its checked-in version is the build-time fallback for
that isolated context. Local/monorepo builds still compare it to the desktop
package and fail on malformed, zero, or mismatched metadata.

Commit the bump before building.

---

## Local release (default)

From the repo root. `RUSTFLAGS=""` is **load-bearing** — it overrides the CLT
Swift rpath baked into `.cargo/config.toml` that would otherwise make the binary
fail to launch on a stock user Mac (#99). `COPYFILE_DISABLE=1` is **also
load-bearing** — see the AppleDouble gotcha below (hit shipping 0.4.0): without
it, the updater `.tar.gz` can silently brick auto-update for every existing
install.

**1. Build app + updater** (universal, Developer-ID-signed, hardened runtime).
**Deliberately omits `--bundles dmg`** — see step 3 below for why; do not add
it back to this command, it will fail on this machine:
```
cd apps/desktop
COPYFILE_DISABLE=1 \
DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer \
RUSTFLAGS="" \
MACOSX_DEPLOYMENT_TARGET=13.0 \
APPLE_SIGNING_IDENTITY="$APPLE_SIGNING_IDENTITY" \
TAURI_SIGNING_PRIVATE_KEY="$(cat ~/.tauri/petal-updater.key)" \
TAURI_SIGNING_PRIVATE_KEY_PASSWORD="" \
  ../../scripts/run-with-source-provenance.sh --require-clean \
    bash -c 'npm ci && CARGO_TARGET_DIR="$PETAL_PROVENANCE_OUTPUT_ROOT/apps/desktop/src-tauri/target" npx tauri build --config src-tauri/tauri.release.conf.json --target universal-apple-darwin --bundles app,updater'
```
(Drop `TAURI_SIGNING_PRIVATE_KEY` and the `updater` bundle if you only want a
DMG and won't feed auto-update. The key itself has no password, but the Tauri
CLI still tries to prompt for one interactively unless
`TAURI_SIGNING_PRIVATE_KEY_PASSWORD=""` is set explicitly — without it, a
non-interactive shell fails with `incorrect updater private key password:
Device not configured (os error 6)` (ENOTTY on the phantom prompt). Always set
it, even to an empty string.)

The provenance wrapper trusts the invoked build command, lockfile-installed
dependencies, compiler/toolchain, and other same-UID processes. Its final
fingerprint detects persistent source writes; it is not a hermetic sandbox
against a malicious command that mutates, consumes, and restores source bytes.

**Apple Events entitlement (#915) — shared-browser-window Open URL.** Unlike
the Sentry DSN below, this one is baked into the committed
`Entitlements.plist`/`Info.plist`, not an env-var secret, so it needs no
extra flag on this recipe. It IS still a hardened-runtime signing concern:
`com.apple.security.automation.apple-events` (paired with
`NSAppleEventsUsageDescription`) lets a background `osascript` child ask
Chrome/Safari for a shared window's URL. `scripts/verify-universal-app.sh`
gates on it (`codesign -d --entitlements :-` must show the key set to
`true`) as part of "Verify pre-publish release guards" below, so a build made
without it fails the gate rather than shipping a silently-broken Open URL
button. If you ever build with a stripped-down `Entitlements.plist` (a
one-off debug build, a fork's own signing setup) and later restore the key,
the app needs a full rebuild AND re-notarization — entitlements are baked
into the code signature, and re-signing changes the signature Apple already
notarized.

**Crash/error reporting (#281) — `PETAL_SENTRY_DSN` build secret.** Same
pattern as `TAURI_SIGNING_PRIVATE_KEY` above: a real release build must embed
the Sentry DSN at build time from a repo secret (`option_env!("PETAL_SENTRY_DSN")`
in `apps/desktop/src-tauri/src/logging.rs`), because a notarized `.app`
launched via `open`/Dock/Spotlight has no shell environment for a runtime var
to land in. The `petal-desktop` project's DSN is below — a DSN is a publishable
client identifier, not a secret (it ships inside every released binary, and
`strings /Applications/Petal.app/Contents/MacOS/desktop | grep -c
'ingest.*sentry.io'` prints it back out of an installed build). To rotate it,
take a new one from Sentry (org `kiruna-labs`, project `petal-desktop` →
Settings → Client Keys) and update the `PETAL_SENTRY_DSN` GitHub Actions
secret. The literal value is deliberately not written in this public repo —
paste it from the Sentry dashboard when running the local recipe:
```
PETAL_SENTRY_DSN="<petal-desktop project DSN from the Sentry dashboard>" \
  ../../scripts/run-with-source-provenance.sh --require-clean \
    bash -c 'npm ci && CARGO_TARGET_DIR="$PETAL_PROVENANCE_OUTPUT_ROOT/apps/desktop/src-tauri/target" npx tauri build --config src-tauri/tauri.release.conf.json --target universal-apple-darwin --bundles app,updater'
```
**Local/CI builds that aren't a real release must NOT set this** — it's
absent by default for every plain `cargo build`/`cargo test`/`tauri dev` run,
which is the correct, silent, no-network-attempt state for contributor
builds. `apps/desktop/src-tauri/build.rs` re-emits it as a tracked
`cargo:rustc-env` (with `cargo:rerun-if-env-changed=PETAL_SENTRY_DSN`) so a
bare env-var change alone correctly invalidates cargo's build cache instead
of silently keeping a stale/absent DSN baked into a cached object — the same
gotcha class as `PETAL_BACKEND_URL` immediately above it in that file. See
`logging.rs`'s module doc comment for the full design (allowlist-first PII
policy, explicit flush-before-death on the panic and ObjC-exception hooks,
`sentry-log` bridge for `log::error!`/`log::warn!`).

**Product analytics — `PETAL_POSTHOG_KEY` build secret.** Same bake as the
Sentry DSN: a notarized `.app` has no shell environment, so the PostHog
project token must be compiled in (`option_env!("PETAL_POSTHOG_KEY")` in
`apps/desktop/src-tauri/src/analytics.rs`, re-emitted from `build.rs`).
Local and CI builds leave it unset — product events simply do not fire, and
no network attempt is made. The token is a `phc_…` project key for the
**Petal** PostHog project (id `317298`, org Kiruna Labs, US cloud); it is
**not** a publishable client identifier in the DSN sense and must never be
committed. Add it as the GitHub Actions secret `PETAL_POSTHOG_KEY` (and pass
it into a local release command the same way you pass `PETAL_SENTRY_DSN`).
The closed event list is `docs/POSTHOG_EVENT_ALLOWLIST.md`. Do not
wire PostHog into the backend. The browser client emits the same events
(minus the native-only `annotation_toggled`) via `web-harness/src/analytics.ts` and bakes
`VITE_PETAL_POSTHOG_KEY` at web-harness build time (Vercel project env, never
git). Local and `scripts/deploy-web-harness.sh --build-only` stay keyless.

**In-app bug reports (#292/#786) — `VITE_USERDISPATCH_PUBLIC_KEY` build var.**
Set alongside `PETAL_SENTRY_DSN`, and for the same reason: the value is baked
in at build time, so a release built without it ships with the feature
compiled off — no bug-report button in the meeting topbar (`Gallery.svelte`),
no trigger on the home screen (`MainMenu.svelte`), and the
`@userdispatch/sdk` module never imported. Unlike the DSN this one is read by
the WEBVIEW, so it must be a Vite `VITE_`-prefixed var rather than a Rust
`option_env!` bake (`apps/desktop/src/lib/feedback/config.ts`), and it must be
the **public** `pk_…` key — the built bundle is readable by anyone with the
app, so a secret `sk_…` value is rejected by the format gate on purpose:
```
PETAL_BACKEND_URL="https://app.petal.live" \
VITE_USERDISPATCH_PUBLIC_KEY="pk_<public key from the UserDispatch dashboard>" \
PETAL_SENTRY_DSN="<petal-desktop project DSN from the Sentry dashboard>" \
PETAL_POSTHOG_KEY="<Petal PostHog project token from project 317298 settings>" \
  ../../scripts/run-with-source-provenance.sh --require-clean \
    bash -c 'npm ci && CARGO_TARGET_DIR="$PETAL_PROVENANCE_OUTPUT_ROOT/apps/desktop/src-tauri/target" npx tauri build --config src-tauri/tauri.release.conf.json --target universal-apple-darwin --bundles app,updater'
```
(`PETAL_BACKEND_URL` is not optional: `build.rs` hard-fails a release build
without it since the 0.8.2 shipped-without-it incident. Confirmed live cutting
0.8.8 — the build aborts, it does not silently bake a blank.)
The web client's copy of the same key is wired separately, in
`web-harness/vercel.json`'s `buildCommand` and `scripts/deploy-web-harness.sh`
— setting it here does NOT reach `meet.petal.live`, and vice versa. As with
the DSN, local/CI builds must not set it; absent is the correct state for
contributor builds. The reliable check that a release actually carries it is
the UI itself: launch the built `.app` and confirm the bug-report button is
present in the home screen header and in a meeting's topbar, right of the
spotlight/layout toggle. (Don't try to grep the bundle — Tauri embeds the
frontend assets inside the binary, so an absent match proves nothing.)

**Known local-build gotchas (all environment-specific, not code bugs — hit
2026-07-05 building 0.3.25):**
- **Stale build cache from the Relay→Petal folder rename.** `cargo build`
  can fail with `failed to read plugin permissions: ... No such file or
  directory` pointing at a `<old-checkout-path>/...` path that no longer
  exists. Fix: `cargo clean -p desktop --release && cargo clean -p tauri
  --release` (inside `src-tauri/`), then rebuild — forces both crates'
  build scripts to regenerate under the current path. Don't `cargo clean`
  the whole tree; that nukes the cached `webrtc-sys`/Swift-crate builds and
  costs ~10+ min to redo.
- **The `hdiutil`/`bundle_dmg.sh` TCC failure is folded into step 3 above as
  the primary recipe now** (confirmed reproducing twice — don't attempt
  `--bundles dmg` first and rediscover it).
- **AppleDouble sidecar files in the updater `.tar.gz` silently brick
  auto-update (hit shipping 0.4.0, 2026-07-05).** If the built `Petal.app`
  carries `com.apple.provenance` (or other) extended attributes, `tar`
  writes a companion `._<name>` AppleDouble entry for every archived file
  *unless* `COPYFILE_DISABLE=1` is set. BSD `tar tzf`'s own listing hides
  these transparently (`tar tzf Petal.app.tar.gz` looked completely clean),
  but Tauri's Rust-based updater plugin extracts every archive member
  literally and fails with `failed to unpack `._Petal.app` into ...` —
  every existing install trying to auto-update sees this as a visible error
  toast. Verify with Python's stdlib `tarfile` (not `tar tzf`), which does
  show every literal member:
  ```
  python3 -c "import tarfile; print([n for n in tarfile.open('Petal.app.tar.gz','r:gz').getnames() if n.split('/')[-1].startswith('._')])"
  ```
  Fixed permanently two ways: (1) `COPYFILE_DISABLE=1` is now in the
  documented build command above; (2) `scripts/publish-blob.mjs` has a
  standing `verifyCleanTarball` gate (same pattern as its universal-binary
  `lipo` gate) that refuses to upload a `.tar.gz` containing any `._`
  sidecar entry — so even a build missing the env var can't reach
  production. If you ever see this error reappear, rebuild with
  `COPYFILE_DISABLE=1` and re-publish; the gate should have caught it before
  you got here.
- **The updater `.tar.gz` from step 1 is UNSTAPLED (found 2026-08-10).** The
  local recipe builds the tarball in step 1 and staples the `.app` in step 2,
  and nothing regenerated the tarball in between — so every locally published
  auto-update artifact carried an app with no notarization ticket, while the
  handed-out DMG was fine. Fixed two ways, same shape as the AppleDouble
  gotcha above: (1) **step 2b** below re-tars and re-signs from the stapled
  `.app`; (2) `scripts/publish-blob.mjs`'s `verifyStapledInsideTarball` gate
  refuses to upload a tarball whose `Petal.app/Contents/CodeResources` ticket
  is absent or is not byte-identical to the one on the app it just validated
  with `xcrun stapler validate`. Presence alone isn't checked, because a
  stale ticket from a previous version would be present and still wrong. Cloud
  releases are unaffected — see step 2b for why.
  **Do not confuse the two `CodeResources` files.** `xcrun stapler staple`
  writes the notarization ticket to `Petal.app/Contents/CodeResources` — a
  binary blob whose first four bytes are `s8ch`. The code signature's own
  resource manifest is `Petal.app/Contents/_CodeSignature/CodeResources`, a
  plist present on every signed build, stapled or not. An UNSTAPLED app has
  the second file and not the first, so "CodeResources is there, so it must
  be fine" is exactly the wrong conclusion. Tell them apart with:
  ```
  head -c 4 Petal.app/Contents/CodeResources   # s8ch -> a real stapled ticket
  ```
- **`strings | grep` on the x86_64 slice can find ZERO matches for a value
  that is genuinely baked in (#874, confirmed cutting 0.9.1, 2026-08-22).**
  On x86_64, LLVM sometimes materializes a string literal (`PETAL_BACKEND_URL`,
  `PETAL_POSTHOG_KEY`, the PostHog host, ...) as a sequence of inline
  `movabs $imm64` immediates instead of one contiguous `.rodata` byte run —
  so the value never exists as a single readable string, and a whole-value
  `strings | grep` reports it missing even though it is compiled in. arm64
  does not do this; the same value on arm64 stays one contiguous string.
  Evidence, reproduced against `/Applications/Petal.app` (0.9.1) while
  fixing this:
  ```
  lipo -thin x86_64 Petal.app/Contents/MacOS/desktop -output /tmp/desktop.x86_64
  python3 - <<'PY'
  v = b'https://app.petal.live'
  d = open('/tmp/desktop.x86_64','rb').read()
  print(d.count(v), [d.count(v[i:i+8]) for i in range(0, len(v), 8)])
  PY
  # -> 0 [29, 2, 2]   <- whole=0 but every 8-byte chunk present == baked
  ```
  The 8-byte chunk width matches a single `movabs` immediate; a genuine bake
  also shows consecutive chunks exactly 14 bytes apart, descending (10-byte
  `movabs` + 4-byte store, emitted in reverse). Do not conclude "not baked"
  from a plain `strings | grep` miss on x86_64 alone — reconstruct from
  8-byte chunks instead, as above.

  This false negative previously corrupted the written record: #681's body
  (2026-08-06) measured the shipped 0.8.3 binary correctly (`grep -c
  'ingest.*sentry.io'` → 2, one hit per slice, because the Sentry DSN
  happens to stay contiguous in both slices) and concluded crash reporting
  was live, while #681's closing comment (2026-08-15) claimed the opposite
  for the same build — the two are reconciled in #874's thread, and the
  body's measurement is the correct one.

  **This is now automated, not just documented.** `scripts/publish-blob.mjs`
  runs a per-architecture gate for the Sentry DSN, the PostHog key, and (new)
  `PETAL_BACKEND_URL`: for every arch `lipo -archs` reports, it `lipo -thin`s
  that slice into a scratch temp file and checks it with
  `publish-blob-lib.mjs`'s `valueIsBakedInSlice` — contiguous OR the ordered
  8-byte-chunk reconstruction above — refusing to publish and naming the
  slice if either is missing. When the expected value isn't available at
  publish time (`PETAL_SENTRY_DSN`/`PETAL_POSTHOG_KEY` are secrets this
  script doesn't otherwise read; keep them exported in the same shell
  session through the publish step for the strongest check), each slice
  still gets a per-slice `strings -a` contiguous-only fallback, and the OK
  line says so explicitly rather than silently under-claiming what was
  checked, e.g.:
  ```
  Sentry DSN gate: OK (arm64: contiguous; x86_64: contiguous) [no expected value in env -- chunked reconstruction skipped, contiguous-only check per slice]
  Backend URL gate: OK (arm64: contiguous; x86_64: chunked 29/2/2)
  ```
  (`PETAL_BACKEND_URL`'s expected value is the constant
  `https://app.petal.live`, so its gate always has a concrete value and
  always runs the full check — no fallback path for it.)

**Confirmed telemetry bake per shipped version.** Whether a release actually
carries the Sentry DSN, reconstructed from real per-slice measurements
(never grep-only) rather than assumed from the release recipe having been
followed. "unknown" means genuinely not measured — never guess a cell.

| Version | Sentry DSN baked | Source |
|---|---|---|
| 0.8.3 | yes | Measured, #681 issue body (2026-08-06): `strings \| grep -c 'ingest.*sentry.io'` on the installed `/Applications/Petal.app` → 2 (one per slice). |
| 0.8.6 | yes | Measured, #681 comment (2026-08-14). |
| 0.9.1 | yes | Measured while fixing #874 (2026-08-24), against the installed `/Applications/Petal.app` (`CFBundleShortVersionString` 0.9.1): `strings -a` on each `lipo -thin`'d slice matches `/ingest.*sentry\.io/i` as a **contiguous** string on both arm64 and x86_64 — present in both slices. |

Every other shipped version: unknown — not measured. Measure the same way
(`lipo -thin` each slice, `strings -a` + the DSN pattern, or the chunk
reconstruction above for a concrete value) before writing a new row; don't
extrapolate from a neighboring version.

Output lands in
`apps/desktop/src-tauri/target/universal-apple-darwin/release/bundle/`:
- `dmg/Petal_<version>_universal.dmg` — the human download
- `macos/Petal.app` + `macos/*.app.tar.gz` (+ `.sig`) — the updater artifact

**2. Notarize + staple the `.app` — BEFORE the DMG exists.**
**Real incident, hit shipping 0.7.0 (2026-07-11):** every release cut by this
doc up to and including 0.6.4 only ever notarized+stapled the outer DMG
container. That's not enough — Apple's notarization ticket is scoped to
whatever gets submitted, so stapling the DMG never attaches a ticket to the
`.app` bundle inside it. A user who mounts the DMG and launches `Petal.app`
gets Gatekeeper checking the `.app` directly, finds no staple, and shows the
"can't verify, override manually" prompt — even though `spctl -t open` on the
DMG itself reports `accepted`. **Staple the `.app` first**, so the DMG (built
from it in step 3) inherits an already-stapled binary:

```
APP=apps/desktop/src-tauri/target/universal-apple-darwin/release/bundle/macos/Petal.app
cd "$(dirname "$APP")"
ditto -c -k --keepParent Petal.app Petal_app_for_notarize.zip
../../scripts/notarize.sh submit Petal_app_for_notarize.zip --wait
xcrun stapler staple Petal.app
rm -f Petal_app_for_notarize.zip
```

**2b. Re-create the updater tarball from the now-stapled `.app`. Not optional.**
Step 1 built `Petal.app.tar.gz` *before* step 2 stapled anything, and step 6
publishes that tarball — so without this step every auto-update user installs
an **unstapled** app, which fails an offline Gatekeeper check even though the
DMG you handed out is perfect. (Confirmed 2026-08-10 by extracting a freshly
built tarball: `does not have a ticket stapled to it`.) The `.sig` must be
regenerated too, or the updater's minisign check fails against the new bytes:
```
cd "$(dirname "$APP")"
rm -f Petal.app.tar.gz Petal.app.tar.gz.sig
COPYFILE_DISABLE=1 tar czf Petal.app.tar.gz Petal.app
npx tauri signer sign -f ~/.tauri/petal-updater.key -p "" Petal.app.tar.gz
```
`COPYFILE_DISABLE=1` is **not** optional — hand-rolling the tarball otherwise
re-enters the 0.4.0 AppleDouble trap documented above. `-p ""` is required or
the CLI blocks on a phantom TTY prompt (the same `os error 6` as step 1).
`scripts/publish-blob.mjs`'s `verifyStapledInsideTarball` gate now refuses to
publish a tarball whose notarization ticket isn't the one stapled to the
`.app` on disk, so a forgotten step 2b fails loudly at publish time instead of
silently on users' Macs.

**Cloud releases do not need this step, and the gate does not block them.**
tauri-bundler runs its whole package-type loop (`app`, which notarizes +
staples inline when `APPLE_ID`/`APPLE_PASSWORD`/`APPLE_TEAM_ID` are set, then
`dmg`) *before* it builds the updater tarball, and that tarball is made from
the stapled `.app` on disk — verified against tauri-cli v2.11.4,
`crates/tauri-bundler/src/bundle.rs`. The local recipe only hits this because
it passes no `APPLE_ID`, so nothing staples during the build at all.

**3. Build the DMG by hand from the now-stapled `.app`.** Do not run
`npx tauri build --bundles dmg` (or `app,dmg,updater` together) on this
machine — its `bundle_dmg.sh` step reliably fails via a real `hdiutil` TCC
wall, **confirmed twice** (2026-07-05 shipping 0.3.25, 2026-07-14 shipping
0.7.8, both times: `hdiutil create -srcfolder ... -volname "Petal"` →
`could not access /Volumes/Petal/Petal.app - Operation not permitted`, a
`copy-helper` daemon TCC denial for whatever process drives the shell, not a
code bug — confirmed via `log show --predicate 'process == "copy-helper"'`
showing `copy error (canceling): <private>: Operation not permitted` at the
same timestamp). It's specific to volume name `"Petal"` + a real signed app
bundle; a differently-named volume or a plain non-privileged
create+mount+`ditto` sequence both work fine, which is exactly what this
recipe does — go straight here instead of attempting the tauri-bundled path
and hitting the failure first:
```
hdiutil create -size 300m -fs HFS+ -volname "PetalRelease" -attach /tmp/petal-dmg-rw.dmg
ditto "$APP" "/Volumes/PetalRelease/Petal.app"
ln -s /Applications "/Volumes/PetalRelease/Applications"
hdiutil detach /Volumes/PetalRelease -force
DMG=apps/desktop/src-tauri/target/universal-apple-darwin/release/bundle/dmg/Petal_<version>_universal.dmg
mkdir -p "$(dirname "$DMG")"
hdiutil convert /tmp/petal-dmg-rw.dmg -format UDZO -o "$DMG"
codesign --force --sign "$APPLE_SIGNING_IDENTITY" "$DMG"
```
The DMG's own codesign step above is easy to forget and separate from the
`.app`'s signature (`bundle_dmg.sh` normally handles it for you, which is one
more reason a skipped/failed dmg bundle step silently leaves this undone) —
`spctl -a -vvv -t open --context context:primary-signature` on an unsigned
DMG reports `rejected / no usable signature` even after a fully correct
notarize+staple cycle on the `.app`. Known cosmetic cost of this recipe: the
DMG's mounted volume label reads "PetalRelease" instead of "Petal" —
everything else (codesign, notarization, stapling, `spctl`, the app itself)
is identical to what `bundle_dmg.sh` would have produced. Worth root-causing
properly (which TCC category, which process needs the grant) if you have
spare cycles, but don't block a release on it — this workaround is
proven-reliable, not a stopgap.

**4. Notarize + staple the DMG** (Apple round-trip, ~1–3 min each):
```
../../scripts/notarize.sh submit "$DMG" --wait
xcrun stapler staple "$DMG"
```

**5. Verify — BOTH the DMG and the `.app` inside it** (see "Verify any build"
below; the `.app`-level check is the one that actually reproduces what a user
experiences, don't skip it). Then hand out the DMG.

**5b. Required release smoke gate** on a test Mac before publishing or handing
out the DMG. Use the script's release env vars so the exact same gate shape is
used locally and in CI:
```
PETAL_RELEASE_APP=/Applications/Petal.app \
PETAL_RELEASE_DMG="$DMG" \
  scripts/release-smoke.sh

PETAL_RELEASE_APP=/Applications/Petal.app \
  scripts/release-smoke.sh --assert-log
```
The first command verifies the signed app/DMG and prints the manual clean-TCC
gate. After you reset/revoke Screen Recording and Accessibility, complete
onboarding's Screen Recording, Microphone, Camera, and Accessibility grants,
relaunch if prompted, join, share, request control, and verify first input lands,
the second command asserts the required `petal.log` markers. The first command
also records a run boundary (the current end of `petal.log`, in
`<log>.release-smoke-baseline`); `--assert-log` only accepts markers logged
AFTER that boundary and fails on a missing boundary or an untouched log, so a
previous session's markers can neither satisfy nor fail the gate (#622). The
share-liveness marker (`share liveness confirmed`) is only emitted after
several affirmatively-changed frames — a frozen share fails it, so actually
move content in the shared window during step 7. For a non-default
Team ID, backend URL, or log path, set `PETAL_RELEASE_TEAM_ID`,
`PETAL_RELEASE_BACKEND_URL`, or `PETAL_RELEASE_LOG`. For the broader live
pass (the marker set that originated in GitHub #28, now closed), add
`--markers-file scripts/release-smoke-issue-28-markers.txt`.

**6. (Optional) feed auto-update** — only if you want existing installs to
update to this local build. `WINDOWS_BUNDLE_DIR` is optional: with it, the
publisher uploads the verified Windows artifact (stage the release workflow's
`.exe` + `.exe.sig`, or a matching pair you built) and writes
`windows-x86_64` into `latest.json`; without it the run prints `windows: none
(macOS-only publish; latest.json will omit windows-x86_64)` and publishes a
macOS-only manifest (a user directive of 2026-08-25 — Windows is not a
release blocker while it is early). `TAG` / `TAG_ANNOTATION` feed the release
notes; omit them and the notes fall back to `Petal v<version>`.
```
cd /path/to/petal
BLOB_READ_WRITE_TOKEN=<vercel blob token> \
VERSION=<version> \
TAG=v<version> \
BUNDLE_DIR=apps/desktop/src-tauri/target/universal-apple-darwin/release/bundle \
WINDOWS_BUNDLE_DIR=/path/to/windows-release-artifact   # optional \
PETAL_VERIFY_DEPLOY_FRESHNESS_SCRIPT="$PWD/scripts/verify-deploy-freshness.sh" \
  node scripts/publish-blob.mjs
```
**Deploy-freshness gate (user directive 2026-08-22: web ships in sync with
native, always).** `publish-blob.mjs` now runs
`scripts/verify-deploy-freshness.sh` as its FIRST gate and refuses to publish
if meet.petal.live or app.petal.live was built from a commit missing any
`web-harness/`/`shared/`/`contracts/` (web) or `backend/`/`contracts/`
(backend) change on `origin/main` — or if either deployment is unreachable.
This is remediation-only, no override: deploy the stale service
(`scripts/deploy-web-harness.sh --prod --yes`; `cd backend && vercel --prod
--yes -e PETAL_DEPLOY_COMMIT=$(git rev-parse HEAD)`), then rerun. Background:
0.9.1 native shipped while meet.petal.live still served a 0.9.0-era build
(`d227ce4d`, missing an honest-telemetry fix) — this gate makes that split
impossible to repeat silently.

**Module-resolution gotcha (hit 2026-07-07):** the repo has no root `package.json`/
`node_modules`, but `scripts/publish-blob.mjs` imports `@vercel/blob` (only
installed under `backend/node_modules`). Node's ESM resolver walks up from the
*importing file's own directory* (`scripts/`), not from `$PWD`, and does **not**
honor `NODE_PATH` for bare specifiers (that's a CommonJS-only mechanism) — so
`node scripts/publish-blob.mjs` fails with `ERR_MODULE_NOT_FOUND: @vercel/blob`
even with a correct `BLOB_READ_WRITE_TOKEN`. Fix once per machine:
```
mkdir -p node_modules/@vercel
ln -s "$(pwd)/backend/node_modules/@vercel/blob" node_modules/@vercel/blob
```
(A root `node_modules/` isn't git-tracked, so this is a local, one-time,
harmless fix — not a repo change.) Get `BLOB_READ_WRITE_TOKEN` itself via
`cd backend && vercel env pull /tmp/petal-blob.env --environment production`
rather than pasting the literal value into a shell command.

This uploads the DMG + signed updater tarball + `latest.json` to Vercel Blob
(served at `https://app.petal.live/api/updater`). The publisher
must only run after `scripts/verify-universal-app.sh` passes — and it does not
trust you: `publish-blob.mjs` re-runs its own gates, in this order, before
writing anything: deploy freshness → universal slices → entitlements → clean
tarball (no AppleDouble) → Sentry DSN baked → PostHog key baked → backend URL
baked → app stapled inside the tarball → version is not a downgrade of the
live manifest. That pre-publish
guard separately verifies both required native slices and the pinned updater
trust anchors in `apps/desktop/src-tauri/tauri.release.conf.json`:
`plugins.updater.endpoints` must be exactly
`["https://app.petal.live/api/updater"]`, and
`plugins.updater.pubkey` must match the baked minisign public key — while the
committed `tauri.conf.json` must keep `endpoints: []` (the gate fails if the
production anchors ever leak back into the open-source default). An arm64-only
build or drifted updater key/endpoint must never feed auto-update.

**Confirm it's actually live** (don't just trust a green `publish-blob` exit —
hit the real endpoint). It's a **flat GET with no path suffix** — Tauri only
appends `{{target}}/{{arch}}/{{current_version}}` segments if the configured
URL contains those placeholders, and this backend's endpoint doesn't, so
`.../api/updater/darwin-aarch64/0.0.0`-style paths 404 (confirmed shipping
0.7.5, 2026-07-12: the plain path below is the only one that resolves):
```
curl -s https://app.petal.live/api/updater | python3 -m json.tool
```
Expect HTTP 200 with `"version"` matching what you just published and
`darwin-aarch64`, `darwin-x86_64`, and `windows-x86_64` platform entries pointing
at existing blob URLs with non-empty Tauri signatures.

**If you drive this build from an automated/background process (e.g. an
agent), run `tauri build` as a single foreground/blocking step — do not
background it and poll for completion separately from the process driving
it.** Confirmed shipping 0.7.5 (2026-07-12): a build was started via a
backgrounded shell command from inside an agent session, and when that
agent's own turn ended before the background job finished, the job was left
orphaned. It had already completed the `app` bundle stage (a fully signed,
correctly versioned `Petal.app` was on disk) but never reached the `dmg`/
`updater` stages — with no error surfaced anywhere, just silently-stale
`dmg/`/`macos/*.tar.gz` output from a prior run sitting there looking
plausible. Always verify `dmg/*.dmg` and `macos/*.tar.gz` mtimes are newer
than your version-bump commit before trusting them.

**Don't invent a synthetic "done" marker to poll for — check the real exit
status.** Confirmed shipping 0.7.12 (2026-07-24): an orchestrating agent
backgrounded the `tauri build` command, then spawned a *second* background
task to watch for completion by `grep`-ing the build log for a literal
`EXIT_CODE=` string. Nothing in the `tauri build`/bundling pipeline ever
writes that string, so the watcher polled forever — even though the build
had actually finished (app signed, updater tarball produced) many minutes
earlier. The parent agent had already exited believing it was still
"waiting," and nothing surfaced an error; the build just sat there looking
unfinished. If you must watch a backgrounded build from another
process/agent, key off the command's own real exit code (e.g. `wait $PID`,
or the task tool's own completion/exit-code field) — never a hand-rolled
log-marker string that isn't part of the tool's actual output contract.

---

## Cloud release (CI)

Pushing a `v*` tag starts a **publishing** run on its own. Use **Actions →
Release → Run workflow** instead when you want a dry run (`publish: false` —
exercises every secret, uploads the signed artifacts to the run, touches
neither the domains nor `latest.json`) or the project's own registered
self-hosted macOS runner (`runner: self-hosted`); don't do both for one tag
or you get two runs serialized by the `release` concurrency group. The
workflow (`.github/workflows/release.yml`, #916) checks out the requested tag
and runs four jobs:

0. **`notary-preflight`** (`macos-26`, ~1 min) — verifies the Apple
   credentials (`notarytool` auth) before any paid build time is spent.
1. **`windows-release`** — the x86-64 NSIS/updater build. The architecture
   gate checks the app binary cargo produced, not the NSIS installer stub
   (the stub is always a 32-bit PE; checking it rejected every release from
   v0.8.5 to v0.9.4).
2. **`deploy-web`** (parallel, `ubuntu-latest`) — preflights the Vercel and
   Blob tokens (read-only `list()`), then stages `backend/` and `web-harness/`
   on Vercel as production deployments **without assigning the domains**
   (`vercel deploy --prod --skip-domain`; web-harness via
   `scripts/deploy-web-harness.sh`, which passes `PETAL_DEPLOY_COMMIT`
   itself). It runs `verify-backend-live.sh`, `verify-web-harness-live.sh`
   (`PETAL_PREPUBLISH=1`, which skips only the checks that compare against
   the not-yet-published manifest) and `verify-deploy-freshness.sh` against
   the staged URLs, through the deployment-protection bypass header.
3. **`release`** (`macos-26`) — builds the universal bundle, imports the Apple
   certificate, signs/notarizes/staples, runs the static smoke gate, downloads
   the verified Windows artifact, then **promotes** the two staged
   deployments (`vercel promote`) so `app.petal.live` / `meet.petal.live`
   serve this commit, re-verifies the real domains, and only then runs the
   publisher that uploads all artifacts and writes the combined `latest.json`
   last. A final `verify-web-harness-live.sh` with `PETAL_EXPECTED_VERSION`
   confirms the manifest, download redirect and web bundle all name the new
   version.

A failure in either platform build, or in the staged web verification,
prevents publication AND leaves production untouched: nothing is promoted
until the native artifacts have passed their guards. The manual "Deploying
the backend" / "Deploying web-harness" sections below are for hotfixes
between releases; a release no longer needs them. The cloud smoke gate is
intentionally static-only; the clean-TCC checklist still requires a real
interactive test Mac and a Windows install/update smoke test should be
recorded by the release operator.

**Dry run — prove the secrets without shipping anything.** Actions → Release
→ Run workflow, enter the tag, and **untick "Publish"**. The run imports the
certificate, notarizes + staples, minisigns, builds Windows and stages both
web services exactly as a real release does, then stops: the staged
deployments are not promoted, `latest.json` is untouched, and the signed
DMG / updater tarball + `.sig` / Windows installer + `.sig` are attached to
the run as an artifact. Tag pushes always publish.

**Secrets.** The full list is the header comment of `release.yml`. Two are
for the web deploys: `VERCEL_TOKEN` (a token scoped to the `kiruna-labs`
Vercel team — any team member can create one under Account → Tokens) and
`VERCEL_AUTOMATION_BYPASS_SECRET` (Vercel's "Protection Bypass for
Automation"; both projects carry SSO deployment protection on every
non-custom-domain URL, so the staged URLs 302 to a login page without it —
the same secret value is provisioned on both projects, so one GitHub secret
covers both). Project ids and the team id are not secrets and live in the
workflow file.
```
git tag v<version>          # e.g. v0.1.1  (must match tauri.conf.json's version)
git push origin v<version>  # this push IS the trigger for a publishing run
```
Watch it under **Actions → Release**. (Use **Run workflow** only for a dry
run or the self-hosted runner, as described above.)

It runs on a paid `windows-latest` runner, two paid `macos-26` jobs
(`notary-preflight` + `release`) and one `ubuntu-latest` job (`deploy-web`)
(~10× billing for macOS; measured 2026-09-03 on `macos-26`: ~20 min for the universal build + bundle, then notarization; the job is capped at 90 minutes because the one hang seen so far — a locked signing keychain — otherwise sits on a password prompt until the 6-hour default). Watch it under
the repo's **Actions** tab; if it fails on signing/notarization/upload it's
almost always one wrong secret value — re-paste that one secret and re-run.

---

## Windows release

The release workflow builds an x86-64 Windows NSIS setup executable on
`windows-latest` and creates its Tauri updater `.sig`. NSIS is intentional:
`apps/desktop/src-tauri/src/updater.rs` validates the downloaded Windows
updater as a PE executable, so MSI is not currently used as the auto-update
payload. ARM64 and MSI distribution remain separate future work.

The current Windows installer is **not Authenticode-signed**. The Tauri `.sig`
provides updater authenticity after installation, but it does not establish a
trusted Windows publisher identity and does not prevent SmartScreen warnings on
the initial download. The release summary and artifact verification explicitly
report `NotSigned`; do not change that documentation until a real certificate
is configured and verified.

### Future owner setup: Authenticode certificate

When the repository owner has a Windows code-signing certificate or signing
service, add it to the Windows job without committing credentials or private
keys:

1. Choose the issuer-approved CI integration. A traditional certificate may be
   supplied as a protected PFX plus password; a hardware/cloud/HSM provider may
   require its own GitHub Action or Tauri `bundle.windows.signCommand`. Prefer
   the provider's OIDC/workload-identity integration when available. Never
   export a hardware-protected private key merely to make CI convenient.
2. Store only the required values as GitHub Actions secrets/variables (for
   example, a base64 PFX and password, or provider account/profile and OIDC
   identifiers). Keep certificate material out of the repository, logs, and
   uploaded artifacts. The exact variable names must match the issuer's
   integration; this repository currently assumes none.
3. Configure signing during the Tauri Windows bundle step so the app payload
   and NSIS installer are Authenticode-signed before the updater `.sig` is
   generated. If bytes change after the Tauri signature is created, regenerate
   the `.sig` from the final installer.
4. Use SHA-256 Authenticode signatures with the issuer's RFC 3161 timestamp
   service. Verify the timestamp and expected publisher/certificate identity
   with `Get-AuthenticodeSignature` (and `signtool verify /pa /tw`) for the
   application payload and final NSIS installer.
5. Replace the current `NotSigned` assertion with a fail-closed `Valid`
   signature check tied to the expected certificate identity, retain the PE
   `0x8664` check, and keep the Tauri `.sig` check. Only then remove the
   SmartScreen warning from user-facing copy and publish documentation.

This setup is intentionally documented rather than configured: the repository
owner may already have a certificate, a CA-managed key, or a cloud signing
account, and the correct CI integration depends on that choice.

### Marketing website follow-up (separate repository)

The `petal-website` repository is not part of this checkout, so its homepage
cannot be changed by this release work. Still open — and while it is, GitHub
#888 stands: the web surfaces offer "Download Petal for Windows" while a
macOS-only publish has no Windows artifact, so that button lands on a JSON
404. The repository owner must update and deploy that project separately:

1. Detect the visitor's operating system for the primary **Download Petal** CTA.
2. Keep explicit fallback links to
   `https://app.petal.live/api/download?platform=macos` and
   `https://app.petal.live/api/download?platform=windows`.
3. Label the Windows option as an x86-64 NSIS installer and disclose that it is
   currently Authenticode-unsigned and may trigger SmartScreen.
4. Deploy the marketing site, then run
   `scripts/verify-deploy-freshness.sh` before the next native release. Do not
   claim website parity until the live homepage serves the new links.

---

## Deploying the backend

**The `backend/` Vercel project deploys SEPARATELY from git — pushing to `main`
does NOT deploy it.** There is no auto-deploy webhook wired up; `backend/api/*`
changes only reach `app.petal.live` (the `petal-backend` project) after an
explicit deploy. This bit twice in one session (2026-07-05): the invite-link
route and a page-copy edit were both correctly committed to `main`, and
`backend/npm test` passed, but the live site kept serving the old build until
someone remembered to deploy.

**Deploy + verify, every time you touch `backend/`:**
```sh
cd backend
vercel --prod --yes -e PETAL_DEPLOY_COMMIT=$(git rev-parse HEAD)
scripts/verify-backend-live.sh          # from the repo root; hits the LIVE deployment
scripts/verify-deploy-freshness.sh      # from the repo root; confirms the deployed commit isn't stale
```
`verify-backend-live.sh` is the check that would have caught both incidents
automatically — it curls production and asserts the actual expected content is
being served, not just that the code exists in the repo. Don't consider a
backend change "done" until this passes. See `docs/TESTING.md`'s Tier 2 section
for what it checks.

The `-e PETAL_DEPLOY_COMMIT=...` flag is load-bearing, not optional: unlike
web-harness (which stamps its own build commit automatically at build time via
`vite.config.ts`), `backend/`'s zero-config serverless functions have no build
step to hook into, so `api/version.ts` can only report the commit it was told
at deploy time. Omitting the flag makes `verify-deploy-freshness.sh` fail
loudly rather than silently trusting an unstamped deploy.

---

## Deploying web-harness

Same gap as the backend: the `web-harness/` Vercel project deploys separately
from git too, and since the 2026-07-08 domain split it also carries a real
serverless function (`api/j.ts`, the join-link interstitial — previously
`backend/api/j.ts`), not just static SPA assets.

**Deploy + verify, every time you touch `web-harness/` (or `shared/`):**
```sh
scripts/deploy-web-harness.sh --prod --yes  # from the repo root
scripts/verify-web-harness-live.sh          # from the repo root; hits the LIVE deployment
scripts/verify-deploy-freshness.sh          # from the repo root; confirms the deployed commit isn't stale
```
**Do not run a plain `cd web-harness && vercel --prod` anymore (#662).**
`vite.config.ts` resolves `@petal/shared` through `web-harness/shared`, a
symlink to the monorepo-root `shared/` package — real for local dev, but
`vercel deploy` uploads only the invocation cwd, and a symlink pointing
outside that cwd resolves to nothing on Vercel's remote build machine. Every
plain `cd web-harness && vercel --prod` deploy since `shared/` was introduced
(96f1bfd3) failed with `Cannot find module '@petal/shared/...'` — silently,
because Vercel never promotes a failed build to the production alias, so
`meet.petal.live` just kept serving a stale build with no visible error.
`scripts/deploy-web-harness.sh` stages a throwaway copy of `web-harness/` in
a system temp dir, dereferences the symlink into a real, self-contained copy
of `shared/` (never duplicated inside the actual repo), and deploys from
there — everything else about the deploy is unchanged, including this
section's `PETAL_DEPLOY_COMMIT` flag, which the script passes for you.

Product events on `meet.petal.live` also need `VITE_PETAL_POSTHOG_KEY` set on
the web-harness Vercel project (Production + Preview). Committing to `main`
does not bake it; `--build-only` stays keyless on purpose.

Changes reach `meet.petal.live` only after this explicit deploy. `vite.config.ts`
stamps `PETAL_DEPLOY_COMMIT` into `/build-info.json` at build time. The flag
is load-bearing, not cosmetic: Vercel's build sandbox does not check out
`.git` for a CLI deploy, so `git rev-parse` inside the build silently fails
without it (confirmed live — the footer had been rendering "dev" in
production before this was added). Note the flag is `-b` (build-time) here,
not `-e` (runtime) as for `backend/` below — Vite bakes this into the static
build, there is no per-request server to hand a runtime env var to.

The live verifier checks the root footer link, compares the updater manifest
version with the deployed JavaScript bundle, and confirms `/api/download`
returns a `302` to `Petal_<version>_universal.dmg`. Because the version text is
written by `main.ts` at runtime, also run the opt-in browser verifier at 320px,
380px, 400px, and 420px widths to confirm the rendered `v<version>` text, full
download label, keyboard focus ring, and absence of horizontal overflow:

```sh
# Start `npm run preview -- --host 127.0.0.1 --port 4173` in web-harness first.
cd web-harness
PETAL_BROWSER_URL=http://127.0.0.1:4173 npm run verify:browser
```

The browser verifier defaults to local preview so offline CI never depends on
production DNS. Set `PETAL_BROWSER_URL=https://meet.petal.live` only for an
explicit live read-only check. It uses Playwright from
`apps/desktop/node_modules/playwright`, or the module path supplied through
`PETAL_PLAYWRIGHT_MODULE`, and can use an existing browser via
`PETAL_CHROME_BIN`.

---

## Verify any build

```
DMG=.../bundle/dmg/Petal_<version>_universal.dmg
# Gatekeeper on the DMG container: must say "accepted / source=Notarized Developer ID"
spctl -a -vvv -t open --context context:primary-signature "$DMG"
# Stapled ticket present on the DMG:
xcrun stapler validate "$DMG"
# Portability (#99): the app binary must carry ZERO CommandLineTools rpath.
#   Check the .app before packaging, e.g.:
otool -l .../bundle/macos/Petal.app/Contents/MacOS/desktop | grep -c CommandLineTools   # -> 0
# Universal release gate (#231/#88) plus updater trust-anchor gate (#102):
# auto-update artifacts must carry both Apple Silicon and Intel slices, and
# tauri.release.conf.json must still point at the pinned updater endpoint/pubkey
# (and tauri.conf.json must still ship an empty endpoint list).
bash scripts/verify-universal-app.sh .../bundle/macos/Petal.app
```

**Do not stop at the DMG-level checks above — they do not prove a fresh
install will work.** `-t open` only exercises the disk-image-mount Gatekeeper
path; it says nothing about the `.app` a user actually launches. Mount the
DMG (or use the built `.app` directly) and check the EXECUTION path too —
this is the check that actually reproduces the real 0.7.0 incident above:
```
hdiutil attach "$DMG" -nobrowse -mountpoint /tmp/petal-verify-mount
spctl -a -vvv -t exec /tmp/petal-verify-mount/Petal.app   # must say "accepted / source=Notarized Developer ID"
xcrun stapler validate /tmp/petal-verify-mount/Petal.app  # must say "The validate action worked!"
hdiutil detach /tmp/petal-verify-mount -force
```

A good release: `spctl -t open` on the DMG **accepted**, `spctl -t exec` on
the mounted `.app` **also accepted** (not just the DMG), `stapler validate`
**worked** on both the DMG and the `.app`, CLT rpath count **0**,
`codesign -dvv` shows `TeamIdentifier=$APPLE_TEAM_ID` and `flags=0x10000(runtime)`.

Then run the clean-TCC release smoke in `scripts/release-smoke.sh`. This is the
gate that catches release-only permission/signature regressions that `tauri dev`
and `scripts/ci-local.sh` cannot see.

## First Real-Intel Validation

Before offering a universal release to Intel users, run the published DMG on a
real Intel Mac:
- Launch from the stapled DMG/app and confirm `~/Library/Logs/Petal/petal.log`
  contains `startup: hardware model=... arch=x86_64 macOS=...`.
- Join a room, share a window, and confirm the log's
  `encoder_implementation` line. If it warns about a software encoder, keep
  CPU Activity Monitor open and test with a smaller shared window.
- Confirm the Network Cockpit does not show "Hardware encoder is unavailable"
  on hardware VideoToolbox, or does show it if Intel falls back to software.
- Record CPU %, the shared window size/fps, and whether typing/scrolling stays
  usable for release notes or follow-up tuning.
