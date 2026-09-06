# Petal backend

Stateless token + room service for Petal. It exists for one reason:
**mint scoped LiveKit JWTs server-side so the LiveKit API secret never ships
inside a client** (the P0 blocker, internal/ISSUES.md #96). It also lets a
client that already holds a room credential check that room's live status
across machines (#98).

**Minimal-backend design (product directive):** the only infrastructure is
**LiveKit + this lean Vercel function — no database.** Rooms live entirely on
LiveKit; the human room name rides in LiveKit room `metadata`. The core
meeting service needs only the three LiveKit credentials; everything else in
the environment (listed under "Environment" below) enables an optional
integration. Favorites/recents and logs/stats stay local on the client.

## Endpoints

| Method | Path | Body | Returns |
|---|---|---|---|
| POST | `/api/token` | `{ room, identity, displayName? }` | `{ url, token, room, displayName? }` |
| POST | `/api/rooms` | `{ name, open?, room? }` | `{ room }` (generates or stamps a credential) |
| POST | `/api/rooms/status` | `{ rooms: [{ room, accessCode? }] }` (credentials you already hold) | `{ rooms: [{ id, name, open, occupancy }] }` — proof-of-possession status, only for the rooms whose credential you sent |
| GET | `/api/rooms` | — | `410 Gone` — the public room directory was removed; use `/api/rooms/status` |
| POST | `/api/gallery-token` | `{ room, identity }` + `Authorization: Bearer <livekit jwt>` | a hidden, subscribe-only token for the in-webview gallery bridge |
| POST | `/api/ai-token` | `{ room, identity }` + `Authorization: Bearer <livekit jwt>` | `{ token, expireTime, model }` |
| POST | `/api/admin` | `{ action: "kick" \| "close", room, identity? }` | `{ ok, action, room }` |
| GET | `/api/version` | — | `{ commit }` (`PETAL_DEPLOY_COMMIT`), read by the deploy-freshness gate |
| GET | `/api/updater` (alias `/latest.json`) | — | the Tauri updater manifest, verbatim from Blob |
| GET | `/api/download?platform=macos\|windows` | — | 302 to the current signed installer's Blob URL |
| GET | `/` | — | 302 redirect to the marketing site (petal.live) |

`room` in the token request is the internal credential
`room-<32 lowercase hex chars>`, derived client-side from the user-facing
`abc-defg-hjk` access code. Bare human names like `eng-sync`, old public
`<label>-<32hex>` credentials, and display labels are rejected and do not mint
tokens. The returned `room` is the LiveKit room name (`petal-room-<credential>`),
and `displayName` is the human room name read from LiveKit metadata when it is
available.
There is no public room directory: `/api/rooms/status` only answers for a
credential the caller already holds, and never returns participant
identities — just whether the room is open and its live participant count.
The create response returns the short access code to the creator. Native clients
that already generated a credential pass it as `room` so the backend stamps that
LiveKit room's metadata instead of creating a separate generated room. For those
native credential stamps, `open` is only an initial value; if server metadata
already exists, the backend preserves its `open` flag and refreshes only the
display label.

Public token requests always mint visible participant tokens with the fixed
Petal participant profile: publish, subscribe, data publish, and own-metadata
updates enabled; `hidden` is always false. Caller-supplied grant fields are
ignored for compatibility and must not be used for hidden gallery/bridge
participants. Hidden subscribe-only tokens require a trusted server-owned path
bound to an already-visible participant.

Token `identity` must be a generated participant id, not a human name. Accepted
forms are the native UUID participant id, the web harness `web-<uuid>` id, and
the legacy `p-...-...` generated fallback. Human-readable names belong in
`displayName`; the backend rejects values such as `alice` or `Jane Doe` as
LiveKit identities so callers cannot trivially mint a token whose authenticated
subject is a teammate's name.

`/api/admin` is the server-side revocation primitive for operators and future
trusted app flows. It requires `Authorization: Bearer <PETAL_ADMIN_TOKEN>` and
never accepts a room credential alone as admin authority. `action: "kick"`
removes one LiveKit participant from the derived room; `action: "close"` deletes
the LiveKit room. This is not user authentication: invite credentials are still
bearer capabilities until Petal grows a real account/device-attestation layer.

## AI chat: `/api/ai-token` (#655)

Mints a **Gemini Live ephemeral token** so a participant can hold an AI
conversation about a shared window. Petal's repo is public, so the Gemini key
can only live here: the client connects its WebSocket straight to Google with
the minted token, media never proxies through this backend, and
`GEMINI_API_KEY` joins `LIVEKIT_API_SECRET` under the "never returns the
secret" rule in `lib/livekit.ts`.

Two auth layers, both required:

1. **Cryptographic.** The caller sends its OWN LiveKit access token as
   `Authorization: Bearer <jwt>`; `TokenVerifier` checks the signature against
   `LIVEKIT_API_SECRET`, and the token's `sub`/`video.room`/`video.roomJoin`
   must match the request. This is stronger than `/api/gallery-token`'s anchor,
   which only proves such an identity exists in the room — here the caller
   must *be* that identity, so nobody can mint against a teammate's identity.
2. **Liveness.** That identity must be currently connected to the room
   (`listParticipants`), so a 24h join token outliving the meeting buys
   nothing.

Minted tokens are single-use, must open their session within 30s, and expire
12 minutes after minting. Rate limits: 6 per identity+room per hour and 60 per
client IP per hour, in their own buckets so an AI-chat burst can never lock a
caller out of `/api/token`. Both upstream calls run under a 4s timeout so the
route always answers inside `vercel.json`'s `maxDuration: 10`.

| Env var | Required | Purpose |
|---|---|---|
| `GEMINI_API_KEY` | to enable AI chat | Gemini Developer API key. **Unset = global kill switch:** every call returns `503 {"error":"AI chat is not configured"}`. |
| `GEMINI_LIVE_MODEL` | no | Model id to mint against; defaults to `models/gemini-3.1-flash-live-preview`. |
| `GEMINI_API_VERSION` | no | Overrides the Gemini API version used for minting. |

The response's `model` field is authoritative — **clients must use it rather
than their own constant**, so rotating a retired preview model is an env
change plus `vercel --prod`, not a client release.

Residual risk, stated deliberately: an ephemeral token constrains the model
and response modality but **not** prompt content, so a modified client can
spend its ~12-minute session on arbitrary audio chat against the key. That is
accepted, bounded by `uses: 1`, the expiry, the two rate buckets, and a
per-key quota plus billing alert on the Google Cloud project.

## Browser-Origin And Burst Limits

The backend accepts native/server requests without an `Origin` header. Browser
callers must come from the configured allowlist:

- default: `https://app.petal.live`, `https://meet.petal.live`
- local development: `http://localhost:<port>` and `http://127.0.0.1:<port>`
- override: comma-separated `PETAL_ALLOWED_ORIGINS`

Disallowed browser origins are rejected before endpoint handlers run. `/api/token`
and `/api/rooms` also have best-effort warm-instance token buckets keyed by
forwarded client IP. This is not a global abuse-prevention service across all
Vercel instances, but it closes the unbounded same-instance polling/minting path
and keeps room/identity data out of limiter logs.

## ⚠️ Lockstep contract

`lib/slug.ts` MUST stay byte-for-byte behaviorally identical to:
- `apps/desktop/src-tauri/src/rooms.rs` — `slugify` / `livekit_room_name_for`
- `shared/logic/meetingCode.ts` — `slugify` / `livekitRoomName`

If they diverge, the same invite credential maps to different LiveKit rooms on
different clients and people never meet. `test/local.ts` asserts the exact values
the native Rust unit tests use. Change one, change all three.

Separately, `normalizeAccessCode`/`credentialForAccessCode` (the access-code ->
credential half of `lib/slug.ts`, used for parsing join links) are also
duplicated in `web-harness/api/_lib/slug.ts` — that project's own join-link
interstitial (`web-harness/api/j.ts`, served at meet.petal.live) needs them but
can't import across Vercel project roots. Keep those two functions in sync too.

## Auto-update & distribution (#104)

Distribution is **private** — no public GitHub releases repo. The release CI
(built separately) signs + notarizes macOS, builds a Windows installer without
Authenticode signing, and uploads artifacts to **Vercel Blob** at fixed pathnames
(no random suffix, so this backend can always find the current download without
a database):

| Blob pathname | What it is |
|---|---|
| `latest.json` | the combined Tauri updater manifest, produced by CI |
| `Petal_<version>_universal.dmg` | the macOS human-facing download |
| `Petal_<version>_universal.app.tar.gz` | the macOS updater artifact |
| `Petal_<version>_universal.app.tar.gz.sig` | macOS Tauri updater signature |
| `Petal_<version>_windows_x86_64-setup.exe` | the unsigned Windows x86-64 NSIS download/updater artifact |
| `Petal_<version>_windows_x86_64-setup.exe.sig` | Windows Tauri updater signature |

`latest.json` contains `darwin-aarch64` and `darwin-x86_64` entries, plus
`windows-x86_64` whenever the publish included a Windows artifact (a
macOS-only publish omits that key). The Windows `.sig` is a Tauri updater
signature; it is not Authenticode signing and does not prevent SmartScreen
warnings.

This backend only **reads** from Blob (`lib/blob.ts`, via `list()`) — CI does
all the writing, this service never uploads anything. Three endpoints serve
that content:

- **`GET /api/updater`** (also reachable at `/latest.json` via a `vercel.json`
  rewrite) — fetches `latest.json` from Blob and returns it verbatim. This is
  what `tauri.conf.json`'s `plugins.updater.endpoints` should point at. Returns
  `204 No Content` if no release has been published yet so the updater treats it
  as "no update available" instead of logging a launch-time endpoint error.
- **`GET /api/download`** — defaults to the latest macOS
  `Petal_<version>_universal.dmg` blob for backward compatibility. Pass
  `?platform=macos` explicitly for the same result, or
  `?platform=windows` for the latest
  `Petal_<version>_windows_x86_64-setup.exe`. Both responses 302-redirect to
  the public Blob URL. Unknown platform values return `400`.
- **`GET /`** — this project is a pure API host (app.petal.live); it just
  302-redirects to the marketing site at `https://petal.live/`.

## Environment

| Variable | Needed for |
|---|---|
| `LIVEKIT_URL`, `LIVEKIT_API_KEY`, `LIVEKIT_API_SECRET` | **Required.** Token minting and every room operation. |
| `PETAL_ALLOWED_ORIGINS` | CORS allowlist for browser callers (comma-separated origins). |
| `PETAL_ADMIN_TOKEN` | `/api/admin` kick/close. Without it the endpoint answers `503`. |
| `BLOB_READ_WRITE_TOKEN` | `/api/updater` and `/api/download` (release distribution; see below). |
| `SENTRY_DSN` | Backend error reporting. |
| `GEMINI_API_KEY`, `GEMINI_LIVE_MODEL`, `GEMINI_API_VERSION` | `/api/ai-token` (AI chat). |
| `PETAL_DEPLOY_COMMIT` | Reported by `/api/version`; set by the deploy script. |

**Distribution env var: `BLOB_READ_WRITE_TOKEN`** (alongside the 3 required
`LIVEKIT_*` ones). Despite the name, this backend
only ever calls `list()` with it — Vercel Blob's `list()` API needs a token
even for read-only access; the actual blob URLs it returns are public CDN
URLs that don't need the token to fetch. Get it from the Vercel project's
Storage → Blob tab (same project, or a linked Blob store) and set it as a
Project env var like the LiveKit ones.

**Optional admin env var: `PETAL_ADMIN_TOKEN`** enables `/api/admin`. Without it,
admin-control requests return `503` and no participant kick or room close is
available through the backend.

## Local dev + test

Needs a local SFU: `livekit-server --dev` (listens on `:7880`, key/secret
`devkey`/`secret`).

```bash
npm install
npm run typecheck
npm test             # offline suites: distribution, privacy, ai-token, rooms-resilience, hardening
npm run test:local   # runs against livekit-server :7880 (real create/list/delete)
```

`test/local.ts` verifies slug lockstep, JWT grants, a real `RoomServiceClient`
round-trip against the running server, and rooms-directory idempotency.

## Deploy (Vercel)

1. New Vercel project, **root directory = `backend/`**.
2. Set Project env vars (server-side, never exposed to clients):
   `LIVEKIT_URL`, `LIVEKIT_API_KEY`, `LIVEKIT_API_SECRET` (from the LiveKit
   Cloud project) + `BLOB_READ_WRITE_TOKEN` (from the project's Blob store —
   see "Auto-update & distribution" above). Still no database to provision.
3. The deployed base URL (`https://app.petal.live`) is what the client bakes
   in as its token endpoint (internal/ISSUES.md #97) and its updater endpoint
   (`/api/updater`, alias `/latest.json` — #103/#104, served from Vercel Blob).
   The marketing site (`https://petal.live`) is a separate repo/project
   (`petal-website`); the browser SPA and join links live at
   `https://meet.petal.live` (`web-harness`).

## Client config (#97)

Desktop clients read `PETAL_BACKEND_URL` and call this service for
`POST /api/token` and `POST /api/rooms/status`; browser harness builds can set
`VITE_PETAL_BACKEND_URL` to the same deployed base URL. LiveKit API secrets stay
in this backend's environment only. Local probe binaries may still use
`LIVEKIT_*` directly for transport diagnostics, but the app join/gallery/rooms
paths do not.
