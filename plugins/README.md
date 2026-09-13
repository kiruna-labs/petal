# Petal plugin system — design and status

This directory holds the plugin SDK (`sdk/`) and the vendored built-in
bundles (`builtins/<id>/bundle.json`, pinned by `builtins/SOURCES.json` to a
commit of `kiruna-labs/petal-plugins`, where plugin **source** lives). The
host runtime that loads plugins lives in `shared/plugin-host/` so the desktop
app and the browser client share one implementation.

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
and community investment, and a vetted registry (later a plugin directory with
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
   Everything that *runs* the registry (signing, publishing, hosting,
   vetting, the plugin directory pages) lives in a separate private
   repository owned by the core team. This repo contains no registry server
   code. **Amended 2026-09-09:** that repository is the website repo,
   `kiruna-labs/petal-website` (private), under `registry/`; the registry
   is a static tree on a petal.live subdomain and the directory is web
   pages, so it shares the site's domain and deploy pipeline. **Naming
   (2026-09-12):** "plugin registry" for the signed tree, "plugin directory"
   for the pages people browse; never "marketplace", nothing is sold. The
   code was never the secret; the signing key lives in a protected GitHub
   environment and never in a repo.
8. **Plugin source lives in its own public repo** (owner, 2026-09-09):
   `kiruna-labs/petal-plugins`. This repo keeps the host runtime, the SDK
   (published to npm as `@petal/plugin-sdk`), the contracts, and the
   **vendored, signed `bundle.json`** of each built-in; it stops carrying
   plugin source. Our own plugins live in the plugins repo as source.
   Community plugins are **pointer files** (author repo, subdirectory,
   commit SHA), built and signed by our CI from the pinned commit; a prebuilt
   bundle is never accepted. Details in §2.13.

### Constraints that shaped the design

- No tenant, team, or org exists. Rooms are unowned bearer capabilities, so
  plugins scope to a **person** or a **meeting**, never a team.
- No dormant code (a standing project rule): every host surface ships with
  a first-party consumer in the same milestone.
- The capability file is one flat allowlist. `tauri.conf.json` shipped
  `csp: null` until #37; it now carries exactly one directive,
  `frame-src 'none'` (see 2.3). Third-party code makes a real sandbox
  mandatory.
- UI text must fit the 400 px main window. Native panel changes need a
  live-exercising test. Shared UI and logic go in `shared/`, never duplicated.
- No hosted defaults baked into a plain clone (same rule as the token
  backend and updater, see `docs/SELF_HOSTING.md`). The registry URL and
  public key are build-time configuration; unset means the registry UI is
  hidden and sideloading still works.

---

## 2. Architecture

### 2.1 Where things live

| Path | What |
|---|---|
| `plugins/sdk/` | `@petal/plugin-sdk`: manifest types, `definePlugin`, the frame-side bridge, a Vite lib-build template |
| `plugins/builtins/<id>/bundle.json` | the vendored registry artifact of each built-in, produced by the plugins repo's CI at the commit in `plugins/builtins/SOURCES.json`; `builtins.test.mjs` pins canonical form and the pin (signatures follow the production key) |
| `kiruna-labs/petal-plugins` (separate repo) | source of our own plugins (`plugins/<id>/`), pointer files for community plugins (`community/`), `build-all.mjs` and the no-secrets build CI that produces every registry bundle |
| `shared/plugin-host/` | host runtime shared by both clients: manifest validation, permissions, protocol, frame loader, rate limits, suggestion logic, settings model |
| `apps/desktop/src/lib/plugins/` | Tauri `HostAdapter` and Svelte surfaces |
| `web-harness/src/plugins/` | browser `HostAdapter` and DOM surfaces |
| `apps/desktop/src-tauri/src/plugins/` | Rust: installed-state store, KV storage, registry verify/install, data bus, metadata state, net fetch, commands |
| `contracts/plugin-registry/` | registry index and bundle schemas plus signed test fixtures, vendored by the registry publisher in the website repo |

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
- A document's own CSP cannot stop that document navigating ITSELF, so the
  frame's `<meta>` policy is not the whole boundary (#37). Two things outside
  the frame close it: the EMBEDDER's `frame-src 'none'`
  (`apps/desktop/src-tauri/tauri.conf.json`, `web-harness/vercel.json`), which
  refuses the navigation, and the host's second-`load` gate
  (`shared/plugin-host/host.ts`), which unloads a plugin whose frame left its
  srcdoc so no further envelope is posted into it. Both, because the first
  depends on deployment config and the second acts one `load` late.
  `web-harness/tests/pluginSelfNavigation.test.ts` drives a real escape
  attempt in a browser and checks what the attacker page received.
- CSP3 exempts `about:srcdoc` from `frame-src` matching, so `frame-src 'none'`
  is meant to refuse the self-navigation without refusing the frame itself.
  That is proven in Chromium by the test above and in **WKWebView** by the Test
  Cockpit's `PLUGIN-BOOT` scenario, which loads the real built-in through the
  real host in a real webview of the shipped binary and requires
  `plugins(host): plugin petal.reactions frame ready` in the host journal
  (`apps/desktop/src-tauri/src/test_cockpit/plugin_boot.rs`). It runs on the
  self-hosted Mac in `nightly-loopback.yml`, which is also the release e2e gate.
  Its `plugin-boot-preflight` record names the policy that was in force, so a
  pass says which CSP it passed under.
- Why an iframe and not a hidden Tauri webview per plugin: a second
  `WebviewUrl::App` webview shares the `tauri://localhost` origin with the app
  (shared storage), needs a capability entry, costs tens of MB each, and has
  no browser equivalent. One iframe is one code path for both clients. Tauri
  init scripts are main-frame only and IPC rejects the `null` origin. The
  rendered sandbox test checks a frame sees no such globals, but it runs in
  Chromium, where there is no Tauri to leak: that is evidence about the host
  page, not proof about WKWebView (#37). What WKWebView IS covered for is
  narrower and separate: `PLUGIN-BOOT` proves the frame boots there at all
  (see the CSP bullet above); no automated test inspects a frame's globals
  inside WKWebView.
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
- The app-level CSP is deliberately one directive. A srcdoc frame INHERITS
  its embedder's policy, so anything beyond `frame-src` would also apply
  inside every plugin frame -- which is why the desktop also sets
  `dangerousDisableAssetCspModification: ["script-src", "style-src"]`:
  Tauri would otherwise inject a nonce/hash `script-src`, and the plugin's
  inline runtime and module would be blocked. Further tightening (M5) moves
  desktop frames to a `petal-plugin://` URI scheme served from the installed
  bundle. The loader is an interface so that swap stays local.

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
- **Delivery is confined to participants who have the plugin installed and
  enabled** (owner decision, 2026-09-08, confirmed on the shipped Reactions
  behavior). A `plugin/<id>` packet arriving at a host with no loaded plugin
  for that id is dropped silently. Built-ins are not special-cased: turning
  one off means the same thing as not having it. What closes the gap is the
  discovery prompt (I-6), and optionally sender-side feedback ("2 of 4 can
  see this") built on the `plugins` metadata adverts. Revisit only if real
  usage shows people surprised that a reaction did not reach someone.

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
- **Rust:** `plugins::bus::start_receiver_for_room` joins the existing
  `start_receiver_for_room` calls in `session/room.rs` as a ninth receiver.
  (The plan's `data_receivers.rs` table refactor was dropped while
  implementing I-3: it moved eight working call sites with cfg gates and a
  `DiagnosticsState` dependency for no behavior change, against the
  one-root-cause-per-PR rule. Revisit only if a tenth receiver appears.) It strips the `plugin/` prefix,
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

**Provenance and one-click off (added after M2, owner request):** every
host-drawn plugin control carries a small puzzle badge, plugin popovers carry
a caption, and both say the same line: "<name> · <source> plugin"
("Reactions · built-in plugin"). Right-clicking either opens a plugin menu
with "Turn off <name>". Shared model and copy:
`shared/plugin-host/provenance.ts`; shared styles:
`shared/ui/plugin-provenance.css` (imported by both clients); desktop menu
`apps/desktop/src/lib/plugins/PluginContextMenu.svelte`, web menu in
`setupPlugins.ts`. Users must always be able to tell what is Petal and what
is a plugin, and get rid of a plugin without hunting through Settings.

Three rules keep that answerable, all of them settled by review on
kiruna-labs/petal#71 — do not undo them piecemeal:

1. **The source is the HOST's record** (`LoadedPlugin.source`), never
   manifest text. Every displayed string built from `manifest.name` alone
   leaves a sideloaded "Reactions" indistinguishable from the built-in one.
   `manifest.name` and button labels are additionally refused if they carry
   C0/C1 controls, newlines, or bidi overrides/isolates
   (`isPrintableDisplayText`) — refused, not sanitized.
2. **A plugin never sizes the host's own caption away.** `validateManifest`
   accepts any positive popover `width`, so `popoverContentSize`
   (`shared/plugin-host/surfaces.ts`) clamps it, the caption wraps, and
   `host.ts` grows the popover when it takes a second line. Measured by
   `web-harness/tests/pluginProvenanceRendered.test.ts` in a real browser
   with the real font — reading the CSS cannot tell "fits" from "clipped".
3. **The badge must be hit-tested.** It carries the `title`; a
   `pointer-events: none` badge produces no tooltip at all, and asserting
   `getAttribute('title')` cannot see the difference.
   `apps/desktop/tests/pluginToolbarRendered.test.ts` hit-tests instead.

Turning a plugin off unloads it immediately. Where it comes back differs by
client and the toast says which: the desktop records the choice and Settings
→ Plugins turns it back on; the browser client has no plugins sheet yet
(I-10), so "off" lasts for that page and the toast says to reload. Never
point that toast at UI a client does not have.

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
- Verify chain (desktop, Rust `plugins::registry`): minisign(index) →
  anti-rollback (`generatedAt` and the signature's `timestamp:` may never go
  backwards for a registry; persisted in `plugins.json`) → sha256 + size →
  minisign(bundle) → manifest id/version equal the index entry → grant = index
  permissions ∩ manifest permissions, known permissions only → stored bundle
  re-hashed on every read. **The public key is compile-time only**
  (`option_env!`); a runtime URL override exists in debug builds only and
  must still verify under the baked key. Registry HTTP uses its own client:
  no redirects, no default headers, body streamed and cut at the cap.
  `shared/plugin-host/registry.ts` is the index MODEL (shape validation,
  installability, updates) consumed by the desktop Settings browser on top of
  the Rust-verified index; the two validators are pinned to each other by
  `contracts/plugin-registry/invalid-index-cases.json`. The web client gets
  its own signature verifier together with its install path (I-6), so no
  unused crypto ships before then. The contract fixtures are produced by the
  registry publisher's signer and verified by the Rust crate in tests.
- `build-all.mjs` in the plugins repo emits the deterministic `bundle.json`
  the publisher consumes. Bundles are produced only by that repo's CI from
  pinned source (§2.13); the publisher never signs a bundle it did not build.
- Update check on meeting join at most once per day; re-consent only when
  permissions grew.

**Registry server side (`kiruna-labs/petal-website`, private, `registry/`):** the
publisher that validates, signs, uploads, and merges the index; the hosting
(`plugins.petal.live` over a blob store that only the publish workflow
writes, never committed site content, since anti-rollback would turn a
`git revert` into a dead registry); the vendored copy of
`contracts/plugin-registry/` with a drift test pinned to an upstream commit;
later the scanner, vetting workflow, and the plugin directory pages. Signing runs in a
protected environment with required reviewers and never on the runner that
built the bundle.

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
through relative `?raw` imports of the vendored
`../../plugins/builtins/<id>/bundle.json`, parsed by
`shared/plugin-host/bundle.ts` (the same parser the desktop uses for
installed bundles). The relative import resolves the same way in the desktop
app, the web dev server, Vercel's staged deploy (the `web-harness/plugins`
symlink is dereferenced next to `shared/` by `scripts/deploy-web-harness.sh`),
and every rendered test that aliases `@petal/shared`. No second alias to keep
in sync. They are preinstalled with source `builtin` and enabled by default,
except the webhook notifier.

**Built-ins are buildless** (decided while implementing I-2): each is one
plain-JS `plugin.js` with no imports, registered through the
`globalThis.__petalRegister` hook the frame runtime installs. Reason: the
clients import them with `?raw`, and a build step before every app build
(including Vercel's remote web build, which has no `plugins/node_modules`)
would be fragile. Third-party plugins use `@petal/plugin-sdk` + Vite and
produce the same single-file shape; `build-all.mjs` (plugins repo) packs both
kinds into `bundle.json`.

**Built-ins are vendored bundles** (I-5c, 2026-09-10). The clients import
`plugins/builtins/<id>/bundle.json`, the very artifact the registry serves,
so a built-in is a preinstalled registry plugin and can later be updated from
the registry with no special path. A built-in bump is a PR here that replaces
the vendored bundle with the plugins repo's CI artifact and updates the
commit in `SOURCES.json`; `plugins/builtins/builtins.test.mjs` fails on a
hand-edited (non-canonical) bundle or a pin that disagrees with the manifest.
Signature files and their verification test follow once the production
registry key exists (owner keygen); until then provenance is the pinned
commit.

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

**Unlisted until launch (owner, 2026-09-12).** The registry and the plugin
directory stay undiscoverable until enough plugins exist to announce them:
the directory is never linked from petal.live's navigation or sitemap and its pages and the
registry origin send `X-Robots-Tag: noindex, nofollow, noarchive` (no
`robots.txt` Disallow, which would advertise the path); and the release
workflows set no `PETAL_PLUGIN_REGISTRY_URL` / `_PUBKEY`, so shipped apps
have no "Get plugins" section. Launch is the reverse: links, header removed,
registry variables set in the release workflows. Test builds and testers get
the URL by hand.

### 2.13 Where plugin source lives (owner decision, 2026-09-09)

Three homes, one artifact:

| Repo | Holds | Why |
|---|---|---|
| `kiruna-labs/petal` (this repo) | host runtime, `@petal/plugin-sdk` source (published to npm, versioned with `apiVersion`), `contracts/plugin-registry/`, this design doc, vendored built-in bundles | the SDK is the contract with the host and mirrors `shared/plugin-host/api.ts`; splitting them would make every API change a two-repo event |
| `kiruna-labs/petal-plugins` (public) | source of our own plugins (`plugins/<id>/`), community pointer files (`community/<id>/plugin.json`), `build-all`, the build CI, the public catalog of what is in the registry | the core repo shrinks; plugin contributors never build the app; curation is public and auditable |
| `kiruna-labs/petal-website` (private, `registry/`) | publisher, signing workflow, registry hosting, scanner, plugin directory pages | the registry is a subdomain and the directory is web pages, so they share the site's domain and deploy pipeline; only the key and hosting credentials are secret, and those live in a protected environment, not in any repo |

**Our plugins** are source in the plugins repo, depending on the published
SDK like any third party. A change to Reactions is a plugins-repo PR, then a
bump PR here that replaces the vendored bundle. Chat (I-7) is the first plugin
written there.

**Community plugins are pointers, not merged source** (the Zed extensions
model): `community/<id>/plugin.json` = `{repo, subdir, commit}`.
- `commit` is a **full SHA**, never a branch or tag; a version bump is a PR
  that changes the SHA, and review is the diff between the two commits.
- The plugins repo's CI checks out that exact commit, builds it in an
  isolated job with **no secrets**, and emits the deterministic
  `bundle.json`. The publisher signs only what that job produced; a
  submitted prebuilt bundle is refused. Every registry artifact is therefore
  reproducible from public source.
- Each community repo is **forked under `kiruna-labs`** at the pinned SHA
  when first accepted, so an upstream deletion or history rewrite cannot
  change or remove what we ship.
- New entries and bumps are published `verified: false` until a core-team
  member has read the diff; `verified: true` is a second, reviewed PR.
- Build inputs from the author's repo (dependencies, scripts) run only in
  that sandboxed job. Lockfiles are required; the build job has no network
  beyond the registry the lockfile pins.

Alternatives considered: merging community source into our repo (we become
the maintainer of every orphaned plugin; contribution friction) and bundle-only
submission, the M3 plan until now (reviewing built output with no link back to
source; weakest for security).

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
- I-5b (website repo, `registry/`) Publisher, hosting, vendored contracts with drift
  test, key runbook. DoD: publish the reactions bundle to a staging origin
  and install it through I-5a.
- I-5c Plugin source repo split (§2.13): create `kiruna-labs/petal-plugins`
  with build-all, CI, and the wave-one plugin sources moved from here;
  publish `@petal/plugin-sdk` to npm; vendor signed built-in bundles under
  `plugins/builtins/` and switch `builtins.ts` to them; community pointer
  format and CI documented in that repo's README. DoD: both clients boot
  built-ins from vendored bundles; a test fails on an unsigned or tampered
  vendored bundle; `docs/PLUGINS.md` describes the source-based submission.
- I-6 Suggestion toast and consent sheet. DoD: rendered tests; sideload
  never prompts.
- I-7 `plugins/chat`, written in the plugins repo. DoD: native-to-web chat
  journey; drawer text-fit tests.

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
- Desktop live: `PLUGIN-N2W-REACT` journey in the test cockpit (to automate).
  Until then, the manual recipe that found two real desktop-only bugs
  (DataCloneError on `$state` proxies; the palette write clobbering the
  `plugins` metadata key): start `web-harness` with
  `VITE_PETAL_BACKEND_URL=https://app.petal.live npx vite --port 5173`,
  launch the desktop with `PETAL_BACKEND_URL=https://app.petal.live npm run
  dev:clean`, join a meeting on both, then
  `node web-harness/tests/fixtures/plugins/live-peer.mjs <access-code> send|wait|debug`
  and read `plugins(host):` lines in `~/Library/Logs/Petal/petal.log*`.
- `scripts/ci-local.sh` gains plugin build, plugin tests, and the browser
  plugin check.

## 5. Risks

- Windows parity: M1 to M4 add no native windows; I-9 edits both
  compositors; I-11 needs a Windows twin or an explicit macOS gate.
- CSP inheritance: srcdoc frames inherit the app CSP. #37 added a `csp` value
  without the scheme loader only because it is `frame-src` alone, which is
  inert inside a plugin frame; any directive that is not, still needs I-12's
  scheme loader in the same PR.
- Data-channel abuse: limits enforced outbound in the broker and inbound in
  both dispatchers; chat history uses direct destinations.
- Metadata churn: coalesce `state.set` to two writes per second.
- The broker is full-privilege: keep `shared/plugin-host` small and test
  denial paths first.
- Two-repo drift: the registry schema is the only coupling; the publisher's
  drift test pins an upstream commit so a schema change is a deliberate
  two-PR event.
- Feature branch vs trunk rule: never hold more than one milestone unmerged.
- Community build supply chain: a pointer entry runs the author's build in
  our CI. Mitigations in §2.13: isolated job, no secrets, lockfiles pinned,
  signing separated from building, forked source, `verified: false` until
  read. Never let the build job and the signing key share a runner.
- Three-repo drift: the SDK is the second coupling after the registry
  schema. Publish it with `apiVersion` in its major version and let the
  plugins repo pin it, so an API change is again a deliberate two-PR event.

Open for later: whether the web client needs the Plugins sheet on the home
screen.

---

## 6. Status

Update this table on the branch. Owner is a GitHub handle or "unassigned".

| Issue | Milestone | Scope | Owner | State |
|---|---|---|---|---|
| I-1 | M1 | shared/plugin-host, plugins/sdk, workspace, build-all, docs stub | seinfish | merged (kiruna-labs/petal#4, 2026-09-07) |
| I-2 | M1 | adapters, surfaces, reactions (local), Settings section | seinfish | merged (kiruna-labs/petal#4); web plugins sheet deferred to I-10 |
| I-3 | M2 | data bus (web + Rust), contracts | seinfish | merged (kiruna-labs/petal#55); live native↔web Reactions smoke passed both directions 2026-09-08 (`web-harness/tests/fixtures/plugins/live-peer.mjs`); cockpit journey `PLUGIN-N2W-REACT` still to automate |
| I-4 | M2 | state + advertisement | seinfish | merged (kiruna-labs/petal#55); post-merge fixes in #70 |
| I-4b | M2 | plugin provenance badge, popover caption, right-click "Turn off" | seinfish | merged (kiruna-labs/petal#71) |
| I-5a | M3 | registry client | seinfish | merged (kiruna-labs/petal#99, 2026-09-10; review fixes: compile-time key, anti-rollback, permission intersection, streaming client; web loads registry installs in I-6) |
| I-5b | M3 | registry publisher + hosting (`kiruna-labs/petal-website` `registry/`) | seinfish | publisher, keygen, signer, vendored contracts + drift guard done 2026-09-08; moved into the website repo 2026-09-09 (petal-website PR #1); hosting, protected publish workflow and review tooling still to do |
| I-5c | M3 | plugin source repo split (`kiruna-labs/petal-plugins`), vendored built-in bundles, community pointer contract | seinfish | plugins repo live 2026-09-10 (reactions source, packer, build CI, `community/README.md`); monorepo vendors `plugins/builtins/` on feature/plugin-system-i5c; still to do: publish `@petal/plugin-sdk` to npm (owner: npm scope), signed vendored bundles once the production key exists |
| I-6 | M3 | suggestion toast + consent sheet | unassigned | not started |
| I-7 | M3 | chat plugin (first plugin written in the plugins repo) | unassigned | not started |
| I-8 | M4 | webhook notifier + net fetch | unassigned | not started |
| I-9 | M4 | window-link + header slot, native button removed | unassigned | not started |
| I-10 | M4 | developer mode | unassigned | not started |
| I-11 | M5 | detached native panel | unassigned | not started |
| I-12 | M5 | app CSP + scheme loader | unassigned | not started |
| I-13 | M5 | frame tap B | unassigned | not started |
| I-14 | M5 | registry scan hook | unassigned | not started |
| I-15 | M5 | analytics allowlist entry | unassigned | not started |
