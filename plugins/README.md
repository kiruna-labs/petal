# Petal plugin system — design and status

This directory holds the plugin SDK (`sdk/`), the first-party plugins
(`reactions/`, `chat/`, `webhook-notifier/`, `window-link/`), and the build
script that packs them (`build-all.mjs`). The host runtime that loads plugins
lives in `shared/plugin-host/` so the desktop app and the browser client share
one implementation.

This file is the **living design document** for the feature branch
`feature/plugin-system`. Edit it on the branch as decisions change; keep the
status table at the bottom current. When a milestone merges to `main`, the
matching rows flip to "merged".

For "how do I write a plugin" see `docs/PLUGINS.md` (grows with M1).

---

## 1. Why

Petal should stay a small, focused app while letting people extend it in
powerful ways. A plugin system does that better than feature accretion:
contributors build plugins instead of forking, the project gains defensibility
and community investment, and a vetted registry (later a marketplace with
security scanning) becomes a distribution channel. Sideloading always works,
especially for development.

### Product decisions (owner, 2026-09-05)

1. **Runtime.** Plugins are JS/TS ES modules running in sandboxed frames. The
   same plugin runs in the desktop app and on meet.petal.live. The manifest
   reserves a `native` slot for a later Rust-hosted WASM tier (hot-path media
   filters, OS access). Design only for now.
2. **First-party plugins, wave one.** Emoji reactions, text chat, a local-only
   webhook notifier, and `window-link`, which **migrates the existing native
   "Open URL" remote-window header button into a default-on built-in plugin**
   so the core actually shrinks.
3. **Propagation.** When a peer uses a meeting-scoped plugin you lack, a
   non-blocking toast offers to install it from the verified registry only.
   Sideloaded plugins never prompt.
4. **Registry ships in the first milestone set.**
5. **Frame tap** (`frames:read`) is designed as a permission now, built later.
6. **Workflow.** Work lands on `feature/plugin-system`; each milestone merges
   to `main` by PR when green. The branch is a review surface, not a fork.
7. **Repo split.** Everything the app needs (host, SDK, built-in plugins,
   client-side install/verify, Settings UI, contracts) lives here, open source.
   Everything that *runs* the marketplace (signing, publishing, hosting,
   vetting, storefront) lives in a separate private repository owned by the
   core team. This repo contains no marketplace server code.

### Constraints that shaped the design

- No tenant, team, or org exists. Rooms are unowned bearer capabilities, so
  plugins scope to a **person** or a **meeting**, never a team.
- `internal/docs/COURSE_CORRECTION.md` §2.4: no dormant code. Every host
  surface ships with a first-party consumer in the same milestone.
- The capability file is one flat allowlist and `tauri.conf.json` has
  `csp: null`. Third-party code makes a real sandbox mandatory.
- UI text must fit the 400 px main window. Native panel changes need a
  live-exercising test. Shared UI and logic go in `shared/`, never duplicated.
- `internal/OPEN_SOURCING.md`: no hosted defaults baked into a plain clone.
  The registry URL and public key are build-time configuration; unset means
  the registry UI is hidden and sideloading still works.

---

## 2. Architecture

### 2.1 Where things live

| Path | What |
|---|---|
| `plugins/sdk/` | `@petal/plugin-sdk`: manifest types, `definePlugin`, the frame-side bridge, a Vite lib-build template |
| `plugins/<id>/` | one first-party plugin each: `manifest.json`, `src/`, `tests/`, `dist/plugin.js` (built) |
| `plugins/build-all.mjs` | builds every plugin and emits a deterministic `bundle.json` per plugin |
| `shared/plugin-host/` | host runtime shared by both clients: manifest validation, permissions, protocol, frame loader, rate limits, suggestion logic, settings model |
| `apps/desktop/src/lib/plugins/` | Tauri `HostAdapter` and Svelte surfaces |
| `web-harness/src/plugins/` | browser `HostAdapter` and DOM surfaces |
| `apps/desktop/src-tauri/src/plugins/` | Rust: installed-state store, KV storage, registry verify/install, data bus, metadata state, net fetch, commands |
| `contracts/plugin-registry/` | registry index and bundle schemas plus signed test fixtures, vendored by the marketplace repo |

`plugins/package.json` is the npm workspace root for the SDK and the
first-party plugins. It is deliberately not the repo root: a hoisted
`node_modules` at the repo root could shadow `apps/desktop` and
`web-harness` dependency resolution, and both keep their own lockfiles.

### 2.2 Package format

`manifest.json`:

```jsonc
{
  "manifestVersion": 1,
  "id": "petal.reactions",          // ^[a-z0-9]+(\.[a-z0-9-]+)+$, ≤64 chars, publisher-prefixed
  "version": "1.0.0",               // strict semver
  "name": "Reactions",              // ≤24 chars so it fits a 400 px row
  "description": "…",               // ≤140 chars
  "apiVersion": 1,
  "minHostVersion": "0.10.0",
  "scope": "meeting",               // "meeting" | "local"
  "entry": "plugin.js",             // one ESM exporting activate(petal) and optional mountSurface(petal, surface)
  "permissions": ["meeting:read", "data:publish", "ui:overlay", "ui:toolbar-button", "ui:popover"],
  "contributes": {
    "toolbarButtons": [{ "id": "react", "label": "React", "icon": "smile", "opens": "popover:picker" }],
    "headerButtons": [],
    "surfaces": { "overlay": { "id": "fx" }, "popover": { "id": "picker", "width": 280, "height": 120 } },
    "settings": []
  },
  "native": null                    // reserved: { "wasm": "...", "abi": "petal-native-v0", "capabilities": [] }
}
```

Validation is pure and unit-tested: `local` scope may not request
`data:publish` or `state:write`; every `contributes.*` entry needs its `ui:*`
permission; unknown permission strings fail; a non-null `native` slot is
accepted but the host reports `hostSupports.native = false`.

**Bundle** is `bundle.json`: `{ "manifest": {...}, "files": { "plugin.js": "<source>" } }`,
at most 2 MB, text only (icons are inline SVG or data URLs). One minisign
signature covers the whole file. Both clients parse it with `JSON.parse`; no
zip reader anywhere.

**On disk (desktop):** `app_data_dir/plugins/plugins.json` records installed
state (version, enabled, source `builtin | registry | dev`, granted
permissions, dev path). Bundles live at `plugins/<id>/<version>/bundle.json`,
per-plugin KV at `plugins/<id>/storage.json` (0600, atomic write, same
pattern as `ai_chat/settings.rs`). Ids and versions are validated before any
path join.

**Browser:** installed state in `localStorage` (`petal.plugins.installed.v1`,
registered for factory reset). Bundles are fetched from the registry CDN and
re-verified on every load; no persistent bundle cache. KV in `localStorage`,
64 KB per plugin.

### 2.3 Sandbox and bridge

- Each enabled plugin's logic runs in one `<iframe sandbox="allow-scripts">`
  inside the trusted host page (the desktop main webview's meeting route, or
  the web client's page). The frame is srcdoc-bootstrapped on an opaque
  origin with its own `<meta>` CSP:
  `default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data: blob:; font-src data:; connect-src 'none'; frame-src 'none'; form-action 'none'; base-uri 'none'`.
  `connect-src 'none'` means all network goes through `petal.net.fetch`.
- Why an iframe and not a hidden Tauri webview per plugin: a second
  `WebviewUrl::App` webview shares the `tauri://localhost` origin with the app
  (shared storage), needs a capability entry, costs tens of MB each, and has
  no browser equivalent. One iframe is one code path for both clients. Tauri
  init scripts are main-frame only and IPC rejects the `null` origin; a
  rendered test asserts `__TAURI_INTERNALS__` is undefined inside a frame.
- UI surfaces that need pixels (panel, popover, overlay) are additional
  sandboxed iframes of the same bundle, wired to the logic frame through a
  host-brokered `MessageChannel`. Button-only surfaces (toolbar button, header
  button, toast) are **declarative**: the host draws them and routes clicks.
- Envelope (`shared/plugin-host/protocol.ts`, pinned in contracts):
  `{v:1, kind:'req', id, method, params}`,
  `{v:1, kind:'res', id, ok:true, result}` or
  `{v:1, kind:'res', id, ok:false, error:{code, message}}` with codes
  `denied | rate-limited | invalid | unavailable | internal`, and
  `{v:1, kind:'evt', event, payload}`. The host matches frames by
  `event.source === iframe.contentWindow`, never by origin.
- Every request passes `permissions.ts` then `rateLimit.ts` before reaching
  the per-client `HostAdapter`.
- No generic `plugin_invoke` Rust command. The host page is already trusted
  and already has `invoke`; Rust gets small named commands like every other
  feature. Rust independently re-checks the two dangerous ones: the
  `plugin_net_fetch` host allowlist and the `plugin_publish_data` own-topic
  prefix.
- `csp: null` today means srcdoc frames get only their own `<meta>` policy.
  App-level CSP tightening (M5) moves desktop frames to a `petal-plugin://`
  URI scheme served from the installed bundle. The loader is an interface so
  that swap stays local.

### 2.4 The SDK

`@petal/plugin-sdk` exports `definePlugin({ activate, mountSurface? })`, the
manifest type, and the frame-side bridge. The `petal` object handed to
`activate`:

| Namespace | Permission | Summary |
|---|---|---|
| `plugin` | none | `id`, `version`, `scope`, granted `permissions` |
| `meeting` | `meeting:read` | `self()`, `participants()`, `room()`, `on('participant-joined' \| 'participant-left' \| 'participant-changed' \| 'phase')` |
| `data` | `data:publish` | `publish(sub, payload, {reliable, to})`, `on(sub, cb)`; topics are auto-namespaced `plugin/<id>[/<sub>]`; the host stamps `sender` from the authenticated LiveKit participant |
| `state` | `state:write` | `set(value)` writes `plugins[<id>].state` (2 KB cap) into own participant metadata; `get(identity)`, `on(cb)` |
| `storage` | `storage` | per-plugin KV: `get`, `set`, `delete`, `keys` |
| `ui` | `ui:*` | `channel(surfaceId)`, `onAction(cb)`, `setButton(id, patch)`, `openSurface`, `closeSurface`, `toast(text)` |
| `shares` | `shares:read` | `list()` and `on(cb)` of `{ownerIdentity, windowId, title, sourceUrl, kind}` |
| `net` | `net:fetch:<host>` or `net:fetch:user-urls` | `fetch(url, init)` through the host |
| `clipboard` | `clipboard:write` | `writeText` |
| `frames` | `frames:read` | reserved, not built |
| `log` | none | `debug`, `info`, `warn`, `error` |

`apiVersion` is negotiated at init; the host refuses a plugin whose
`apiVersion` exceeds its own. Additive host features advertise through
`hostSupports`.

### 2.5 Scope: local vs meeting

- **`local`**: never advertised, cannot publish data or state. Gets
  `meeting:read` phase events (that is all the webhook notifier needs).
- **`meeting`**: while enabled, advertised in the participant's LiveKit
  metadata under a `plugins` key:
  `{"petal.reactions": {"v": "1.0.0", "src": "builtin", "state": {...}}}`.
  Metadata was chosen over a heartbeat topic because it is state (late
  joiners see it instantly), rides the existing `ParticipantMetadataChanged`
  path on both clients, and costs no bandwidth at rest. Rust `publisher.rs`
  carries the `plugins` map through every metadata rewrite, the same
  non-destructive pattern as `petalWindowZOrder`. Caps: 2 KB per plugin,
  8 KB total.
- Nothing is installed "meeting-wide"; there is no server state to hold it.
  Propagation is the peer prompt below.

**Suggestion rule** (`shared/plugin-host/suggest.ts`): on a peer metadata
change, for each advertised id whose `src` is `registry` or `builtin`, that is
not installed locally (disabled means the user decided, so no prompt), that
appears in the **verified registry index**, and is not in the dismissed list,
show one actionable toast per id per meeting: "Alex is using Reactions."
with Install and Not now. `src: "dev"` never prompts. A `plugin/<id>` data
packet for an uninstalled id goes through the same gate as a fallback.

### 2.6 Data bus

- **Web:** the if/else dispatcher in `web-harness/src/connection.ts` becomes
  `web-harness/src/dataTopics.ts` with `registerTopic`, `registerTopicPrefix('plugin/')`,
  and `dispatch`. Existing topics register exactly; behavior is unchanged.
- **Rust:** the eight `start_receiver_for_room` calls in `session/room.rs`
  move into a table in `data_receivers.rs`, plus a ninth entry
  `plugins::bus::start_receiver_for_room`. It strips the `plugin/` prefix,
  parses `<id>[/<sub>]`, rate-limits per `(sender, pluginId)`, and emits a
  global `plugin-data` event `{topic, pluginId, sub, senderIdentity, senderName, payloadBase64}`.
  Payload identity fields are never read; the sender is always the
  authenticated LiveKit participant, the same rule `telepointer.rs` and
  `draw.rs` already follow.
- **Contracts:** `contracts/petal-contracts.json` gains
  `topics.pluginPrefix`, `pluginTopicVectors`, `pluginDataEvent`,
  `pluginStateMetadata`, and `pluginLimits`
  (`maxPayloadBytes 16384`, `lossyPerSecond 30`, `reliablePerSecond 10`,
  `inboundPerSenderPerSecond 60`), pinned by Rust, web, and backend tests.
  `docs/CONTRACTS.md` gets a "Plugin bus" section.

### 2.7 UI surfaces

| Surface | Desktop | Web | Shared |
|---|---|---|---|
| Toolbar button | `Gallery.svelte` control bar and pill overflow | `controls.ts` | button model and fit rules in `shared/plugin-host/surfaces.ts`; icons in `shared/ui/icons.ts` |
| Popover | anchored through `shared/ui/dismissibleLayer.ts` | same | frame loader |
| Overlay | transparent `pointer-events: none` iframe over the gallery or pill | `tiles.ts` container | same |
| Panel (drawer) | right drawer, 320 px, gallery mode only; pill mode shows a badge | right drawer | `PluginDrawer.svelte` and `plugin-drawer.css` |
| Header button | `RemoteWindowHeader.svelte` reserved slot, fed by `window.__petalPluginHeaderButtons` from Rust; click invokes `plugin_header_action` | `remoteWindowHeader.ts`, click goes to the broker | button model, label fit (14 chars, icon-only under 520 px) |
| Toast | existing toast host | existing shared toast | `Toast.svelte` |
| Settings | new "Plugins" section in `Settings.svelte`: installed list, permissions, enable/disable, Remove, "Get plugins", Developer mode with sideload path or URL | Plugins sheet from the home-screen menu | `settingsModel.ts` |

The chat panel is an in-window drawer in wave one. A detached native panel
(pattern `ai_chat/panel.rs`) is M5 and carries its own live-exercising test.

### 2.8 Permissions

`meeting:read`, `data:publish`, `state:write` (both meeting scope only),
`storage`, `ui:toolbar-button`, `ui:header-button`, `ui:overlay`,
`ui:popover`, `ui:panel`, `ui:settings`, `ui:toast`, `shares:read`,
`clipboard:write`, `net:fetch:<host>` (exact host or `*.example.com`),
`net:fetch:user-urls`, `frames:read` (reserved; refused with "not supported
by this host").

`net:fetch:user-urls` means the plugin never picks the host. Its
`contributes.settings` declares a `{type: "url", netAllow: true}` field, the
host renders it, stores the value under a host-owned key, and adds that
origin to the runtime allowlist. On web, user-URL webhooks use `no-cors`
POST, so the plugin cannot read the response; that limitation is documented.
`ui:toast` is limited to one per two seconds and 80 characters.

### 2.9 Registry

The registry is a **static, signed tree** at any origin:
`<REGISTRY_URL>/index.json` with `index.json.minisig`, and
`<REGISTRY_URL>/plugins/<id>/<version>/bundle.json` with its `.minisig`.
Versioned paths, index written last, republishing an existing version is
refused.

`index.json` shape: `{schemaVersion, generatedAt, plugins: [{id, name,
description, publisher, latest, versions: [{version, minHostVersion,
apiVersion, permissions, bundleUrl, sigUrl, sha256, size, verified,
scan}]}]}`. Entries with `verified: false` are listed but not installable,
which is the hook for the later security scanner.

Signing uses a **new minisign keypair**, separate from the updater key, so a
compromise of one does not reach the other. The private key never enters any
repository.

**This repo (app side):**
- Build config `PETAL_PLUGIN_REGISTRY_URL` and `PETAL_PLUGIN_REGISTRY_PUBKEY`
  (desktop, through `build.rs`) and `VITE_PETAL_PLUGIN_REGISTRY_URL` and
  `_PUBKEY` (web). Forks point at their own registry.
- Verify chain in both clients: minisign(index), sha256 match,
  minisign(bundle), manifest id and version equal the index entry, manifest
  validates, `minHostVersion` satisfied. Rust uses `minisign-verify`
  (already a dependency); web uses `shared/plugin-host/minisign.ts`.
- `plugins/build-all.mjs` emits the deterministic `bundle.json` the publisher
  consumes, so a third-party developer only ever produces `bundle.json`.
- Update check on meeting join at most once per day; re-consent only when
  permissions grew.

**Marketplace repo (private, core team):** the publisher that validates,
signs, uploads, and merges the index; the hosting project; the vendored copy
of `contracts/plugin-registry/` with a drift test pinned to an upstream
commit; later the scanner, vetting workflow, and storefront.

### 2.10 Install flows

- **Registry:** Settings → Get plugins → Install, or the suggestion toast →
  consent sheet with one plain line per permission (rendered test at 400 px)
  → install → boot immediately if in a meeting.
- **Sideload (Developer mode):** a folder path (desktop) or a
  `http(s)://localhost:*/` URL (both). Hot reload: Rust polls mtimes and
  emits `plugin-sideload-changed`; web polls `ETag`. Dev plugins show a "Dev"
  chip, are granted their declared permissions after one consent, never
  advertise as registry, and are dropped on factory reset.
- **Enable/disable** tears down frames and removes the metadata key.
  **Uninstall** removes files and KV after confirmation. Built-ins can be
  disabled, not uninstalled.

### 2.11 Wave-one plugins

| Plugin | id / scope | Permissions | Wire | UI |
|---|---|---|---|---|
| Reactions | `petal.reactions` / meeting | meeting:read, data:publish, ui:toolbar-button, ui:popover, ui:overlay | `plugin/petal.reactions/emoji`, lossy, `{e, t}`, 4 per second per sender | "React" button opens an 8-emoji popover; overlay floats the emoji with the sender's first name |
| Chat | `petal.chat` / meeting | meeting:read, data:publish, storage, ui:toolbar-button, ui:panel, ui:toast | `plugin/petal.chat/msg`, reliable, `{id, text ≤2000, t}`; a joiner sends `history-req` and peers answer directly with the last 50 | "Chat" button with unread badge opens the drawer; toast while closed |
| Webhook notifier | `petal.webhook-notifier` / local | meeting:read, storage, net:fetch:user-urls, ui:settings | none | settings surface with URL and "Send test"; posts `{event, room, count, at}` for meeting started, ended, participant joined; off until a URL is set |
| Window link | `petal.window-link` / local | shares:read, ui:header-button | none | header button "Open URL", hidden when the share has no source URL; the native button is removed in the same PR |

Built-ins are compiled into both clients by `shared/plugin-host/builtins.ts`
through relative `?raw` imports (`../../plugins/<id>/plugin.js`), which
resolve the same way in the desktop app, the web dev server, Vercel's staged
deploy (the `web-harness/plugins` symlink is dereferenced next to `shared/` by
`scripts/deploy-web-harness.sh`), and every rendered test that aliases
`@petal/shared`. No second alias to keep in sync. They are preinstalled with
source `builtin` and enabled by default, except the webhook notifier.

**Built-ins are buildless** (decided while implementing I-2): each is one
plain-JS `plugin.js` with no imports, registered through the
`globalThis.__petalRegister` hook the frame runtime installs. Reason: the
clients import them with `?raw`, and a build step before every app build
(including Vercel's remote web build, which has no `plugins/node_modules`)
would be fragile. Third-party plugins use `@petal/plugin-sdk` + Vite and
produce the same single-file shape. `build-all.mjs` still packs built-ins
into `bundle.json` for registry publishing.

**M1 storage note:** the enabled map and per-plugin KV live in
`localStorage` on both clients for now (`shared/plugin-host/settingsModel.ts`
keys, cleared by factory reset). The desktop moves installed bundles and KV
to the Rust-owned files described in §2.2 with the registry client (I-5a).

### 2.12 Native tier and frame tap (design only)

- Manifest `native: {wasm, abi: "petal-native-v0", capabilities}`. A later
  Rust wasmtime host instantiates a component whose imports mirror the JS
  API and whose exports are hot-path hooks such as `on_frame` and `on_data`.
  The same permission strings gate both tiers; one plugin's JS frame and
  WASM instance share KV and topic namespace.
- **Frame tap A, host-sampled:** Rust taps the decoded or captured frame of a
  consented window, downscales to 320 px at 5 fps or less, and hands it to
  WASM by pointer or to JS as base64 (JS path limited to 2 fps). Per-window
  consent; the sharer sees who is sampling in metadata.
- **Frame tap B, recommended for JS plugins:** the host extends the existing
  hidden gallery-bridge participant to subscribe to the consented track at
  the lowest simulcast layer, draws to an offscreen canvas, and posts a
  transferable `ImageBitmap` into the frame. No token reaches the plugin and
  no new endpoint is needed. Handing a plugin its own LiveKit token is not
  recommended: it gives sandboxed code a room credential.

---

## 3. Milestones

Each milestone merges to `main` with a real consumer. Issues carry a
Definition of done and the usual labels.

**M1 — Host core, Reactions (local echo), Settings section**
- I-1 `shared/plugin-host` (manifest, permissions, protocol, frame loader,
  rate limit), `plugins/sdk`, root workspace, `build-all.mjs`,
  `docs/PLUGINS.md`. DoD: unit tests; sandbox-escape rendered test.
- I-2 Desktop and web adapters, surfaces (toolbar button, popover, overlay,
  toast), `plugins/reactions` with local echo, Settings "Plugins" list with
  enable/disable. DoD: reactions animate locally in both clients; 400 px
  rendered tests; `ci-local.sh` steps added.

**M2 — Bus, meeting-wide Reactions, advertisement**
- I-3 `dataTopics.ts`, `data_receivers.rs`, `plugins::bus`,
  `plugin_publish_data`, contracts. DoD: reactions cross native and web in a
  `PLUGIN-N2W-REACT` cockpit journey; contract tests on every side.
- I-4 `petal.state`, metadata merge, `plugin-state-changed`, advertisement.
  DoD: metadata fixture tests.

**M3 — Registry, suggestion prompt, Chat**
- I-5a (this repo) Registry client: fixtures, Rust `plugins::registry`, web
  minisign, URL and pubkey plumbing, "Get plugins", update check. DoD:
  install the fixture plugin from a local static server in both clients.
- I-5b (marketplace repo) Publisher, hosting, vendored contracts with drift
  test, key runbook. DoD: publish the reactions bundle to a staging origin
  and install it through I-5a.
- I-6 Suggestion toast and consent sheet. DoD: rendered tests; sideload
  never prompts.
- I-7 `plugins/chat`. DoD: native-to-web chat journey; drawer text-fit tests.

**M4 — Local plugins, developer mode**
- I-8 `plugins/webhook-notifier`, `plugin_net_fetch`, `net:fetch:user-urls`,
  settings surface. DoD: Rust allowlist tests; localhost receiver test.
- I-9 `plugins/window-link`, header slot in both header implementations and
  both compositors, `plugin_header_action`, native Open URL button removed.
  DoD: header rendered tests in both clients; live check on a real remote
  window; no regression in open-URL behavior.
- I-10 Developer mode: sideload path or URL, hot reload, Dev chip. DoD:
  browser e2e with a fixture plugin.

**M5 — Hardening**
- I-11 Detached native plugin panel for pill mode, Windows twin. DoD:
  live-exercising test.
- I-12 App-level CSP and `petal-plugin://` scheme loader. DoD: every existing
  panel route still renders.
- I-13 Frame tap B and `frames:read` consent. I-14 Registry scan hook.
  I-15 PostHog `plugin_installed` allowlist entry.

## 4. Verification

- Unit tests with `node --test` and tsx: manifest, permissions, bridge
  (fake frames, denied and rate-limited paths), minisign, suggestion logic,
  per-plugin reducers. Rendered tests at 400 px for Settings, consent sheet,
  and toast.
- Rust: `timeout 900 cargo test --lib` to a log file, check the
  `test result:` line. Bus topic vectors, registry parse and verify,
  metadata merge, receiver table completeness.
- Contracts pinned on Rust, web, and backend.
- Browser e2e: `scripts/verify-web-harness-browser.mjs` loads a fixture
  plugin by sideload URL and asserts the frame has no Tauri internals and no
  network.
- Desktop live: `PLUGIN-N2W-REACT` journey in the test cockpit.
- `scripts/ci-local.sh` gains plugin build, plugin tests, and the browser
  plugin check.

## 5. Risks

- Windows parity: M1 to M4 add no native windows; I-9 edits both
  compositors; I-11 needs a Windows twin or an explicit macOS gate.
- CSP inheritance: srcdoc frames inherit a future app CSP, so I-12 lands the
  scheme loader in the same PR as any `csp` value.
- Data-channel abuse: limits enforced outbound in the broker and inbound in
  both dispatchers; chat history uses direct destinations.
- Metadata churn: coalesce `state.set` to two writes per second.
- The broker is full-privilege: keep `shared/plugin-host` small and test
  denial paths first.
- Two-repo drift: the registry schema is the only coupling; the marketplace
  drift test pins an upstream commit so a schema change is a deliberate
  two-PR event.
- Feature branch vs trunk rule: never hold more than one milestone unmerged.

Open for later: built-ins updatable from the registry (recommended, M5);
whether the web client needs the Plugins sheet on the home screen.

---

## 6. Status

Update this table on the branch. Owner is a GitHub handle or "unassigned".

| Issue | Milestone | Scope | Owner | State |
|---|---|---|---|---|
| I-1 | M1 | shared/plugin-host, plugins/sdk, workspace, build-all, docs stub | seinfish | done on branch (2026-09-05) |
| I-2 | M1 | adapters, surfaces, reactions (local), Settings section | seinfish | done on branch (2026-09-05); web plugins sheet deferred to I-10 |
| I-3 | M2 | data bus (web + Rust), contracts | unassigned | not started |
| I-4 | M2 | state + advertisement | unassigned | not started |
| I-5a | M3 | registry client | unassigned | not started |
| I-5b | M3 | marketplace publisher + hosting (private repo) | unassigned | not started |
| I-6 | M3 | suggestion toast + consent sheet | unassigned | not started |
| I-7 | M3 | chat plugin | unassigned | not started |
| I-8 | M4 | webhook notifier + net fetch | unassigned | not started |
| I-9 | M4 | window-link + header slot, native button removed | unassigned | not started |
| I-10 | M4 | developer mode | unassigned | not started |
| I-11 | M5 | detached native panel | unassigned | not started |
| I-12 | M5 | app CSP + scheme loader | unassigned | not started |
| I-13 | M5 | frame tap B | unassigned | not started |
| I-14 | M5 | registry scan hook | unassigned | not started |
| I-15 | M5 | analytics allowlist entry | unassigned | not started |
