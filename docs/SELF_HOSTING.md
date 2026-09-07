# Self-hosting Petal

Petal can run against infrastructure you operate yourself. The supported path
is a self-hosted LiveKit server plus a deployment of this repository's
stateless `backend/` service. The backend mints short-lived, room-scoped
LiveKit tokens; the LiveKit API secret stays on the backend and is never baked
into the desktop app or sent to a browser.

There are no application-code changes needed for this setup. Configure the
backend, then build the desktop client with its backend URL embedded at build
time.

## Architecture

You need three pieces:

1. A reachable LiveKit server, with a DNS name and trusted TLS certificate for
   production clients. Clients connect to its WebSocket URL, usually
   `wss://livekit.example.com`.
2. The `backend/` directory deployed as serverless functions. Vercel is the
   supported deployment target, but another platform can be used if it can
   run the Vercel-style `@vercel/node` handlers in `backend/api/`.
3. A desktop build configured with the public base URL of that backend.

The backend uses LiveKit itself as its only room store. It does not require a
database, Redis, or a separate Petal service for the core token and room
discovery APIs.

## 1. Configure LiveKit

Create one API key and secret in the LiveKit server configuration. For a
production self-hosted server, use the LiveKit deployment's normal config
file or `LIVEKIT_CONFIG` mechanism; the exact surrounding settings depend on
whether LiveKit is running in Docker, Kubernetes, or directly on a host. The
important part is that the key and secret match the values supplied to the
Petal backend.

For example, a LiveKit config can contain:

```yaml
keys:
  petal_key: replace-with-a-long-random-secret
```

Here `petal_key` is the API key and its value is the API secret. Keep both
private on the server. Expose the LiveKit signaling endpoint over `wss://` and
configure the network, firewall, UDP/TCP media ports, and TURN as required by
the [LiveKit self-hosting deployment guide](https://docs.livekit.io/transport/self-hosting/deployment/).
`ws://localhost:7880` with
`livekit-server --dev` is suitable only for local development.

The backend needs these environment variables, using the same credentials:

```dotenv
LIVEKIT_URL=wss://livekit.example.com
LIVEKIT_API_KEY=petal_key
LIVEKIT_API_SECRET=replace-with-a-long-random-secret
```

`LIVEKIT_URL` is the public LiveKit signaling URL. The backend also derives the
HTTPS API host from it for room administration, so use the public `ws://` or
`wss://` URL without a path or query string.

## 2. Deploy `backend/`

### Vercel

Create a Vercel project whose root directory is `backend/`, then set the three
`LIVEKIT_*` variables above as server-side Project Environment Variables. Do
not expose `LIVEKIT_API_SECRET` as a client or `VITE_*` variable.

From a checkout of this repository, the project can also be deployed with the
Vercel CLI from the backend directory:

```sh
cd backend
npm install
vercel --prod
```

The deployment's base URL is the value to use for `PETAL_BACKEND_URL` below.
The backend exposes `/api/token`, `/api/rooms/status`, and the other handlers
listed in [`backend/README.md`](../backend/README.md) (`GET /api/rooms`, the
old public room directory, is gone and answers `410`). Browser callers must also be
allowed by CORS. Set `PETAL_ALLOWED_ORIGINS` to a comma-separated list such as
the origin of your web harness:

```dotenv
PETAL_ALLOWED_ORIGINS=https://meet.example.com
```

Native desktop requests do not need this setting because they normally have no
`Origin` header.

The core meeting service needs only the three `LIVEKIT_*` variables. The
following are optional integrations, not prerequisites for self-hosted
meetings:

- `PETAL_ADMIN_TOKEN` enables the protected `/api/admin` kick/close operations.
- `SENTRY_DSN` enables backend error reporting.
- `BLOB_READ_WRITE_TOKEN` enables `/api/updater` and `/api/download` for the
  repository's Vercel Blob-based release distribution. If you do not operate
  those distribution endpoints, they are not needed for token or room APIs.
- `GEMINI_API_KEY` enables AI chat via `/api/ai-token`. See the next section.
- `PETAL_DEPLOY_COMMIT` is reported by `GET /api/version`; the release
  tooling's deploy-freshness gate reads it, but meetings don't need it.

For a non-Vercel host, deploy the same `backend/api/*.ts` handlers with a
compatible serverless adapter and provide the same environment variables.
`backend/vercel.json` supplies the root and updater rewrites used by Vercel;
recreate any routes you need on the alternative platform.

### AI chat (Gemini Live) — optional

Petal's AI chat feature holds a Gemini Live conversation about a shared
window. Because this repository is public, no Gemini key is or ever will be
embedded in a build: the backend holds the key and `POST /api/ai-token` mints
a short-lived **ephemeral token** that the client takes directly to Google.
Audio never proxies through the backend, and the key never reaches a client.

| Variable | Required | Purpose |
| --- | --- | --- |
| `GEMINI_API_KEY` | to enable AI chat | Gemini Developer API key from Google AI Studio. Server-side only — never a `VITE_*` or client variable. |
| `GEMINI_LIVE_MODEL` | no | Live model id to mint against. Defaults to `models/gemini-3.1-flash-live-preview`. |
| `GEMINI_API_VERSION` | no | Overrides the Gemini API version used for minting. Leave unset unless Google moves the endpoint. |

```dotenv
GEMINI_API_KEY=your-gemini-developer-api-key
GEMINI_LIVE_MODEL=models/gemini-3.1-flash-live-preview
```

**Leaving `GEMINI_API_KEY` unset is the supported way to run without AI
chat**, and is also the kill switch for a deployment that already had it:
unset the variable, redeploy, and every `/api/ai-token` request answers `503`
with `{"error":"AI chat is not configured"}`, which clients render as
"AI chat temporarily unavailable" rather than a generic failure. No other
endpoint is affected.

`GEMINI_LIVE_MODEL` exists because Live models are preview products that get
renamed and retired on short notice. The endpoint returns the resolved model
id in its response and clients use that value, so changing the model is an
environment change plus a redeploy — never a client release. Do not pin a
model id in a client build.

Costs are billed to whoever owns the key, so a self-hosted deployment is
paying for its own users' sessions. The endpoint bounds this with layered
checks rather than one:

- The caller must present their own LiveKit access token as
  `Authorization: Bearer <jwt>`; it is verified against `LIVEKIT_API_SECRET`,
  and its identity and room must match the request body.
- That identity must also be **currently connected** to the room, checked
  against the live LiveKit participant list.
- Minted tokens are single-use, must open their session within 30 seconds,
  and expire 12 minutes after minting.
- Two best-effort in-memory rate limits apply: 6 tokens per identity per room
  per hour, and 60 per client IP per hour. Like the other limiters in this
  backend these are per warm serverless instance, not global.

Those bounds are not a substitute for a budget: **set a billing alert and a
per-key quota on the Google Cloud project that owns the key.** An ephemeral
token constrains the model and response modality but not conversation
content, so a modified client can spend its session on arbitrary audio chat.

## 3. Build the desktop client

`apps/desktop/src-tauri/build.rs` reads `PETAL_BACKEND_URL` and embeds it in
the desktop binary. Set it when building from `apps/desktop`:

```sh
cd apps/desktop
PETAL_BACKEND_URL=https://petal-backend.example.com npm run tauri build
```

Use the repository's normal release command and signing variables when making
a distributable release; the important self-hosting setting is still the
same `PETAL_BACKEND_URL` environment variable at build time. The value should
be the backend origin only, without `/api/token`; the app appends endpoint
paths itself.

Copied invite links default to `https://meet.petal.live/<label>/<code>`. If
you host your own browser peer (`web-harness/`), set
`VITE_PETAL_INVITE_ORIGIN=https://meet.example.com` in the same build
environment so the desktop app's invite links point at your deployment.

**There is no hosted default.** A build that leaves `PETAL_BACKEND_URL` unset
bakes no backend at all. A **release** build refuses to compile in that state
(`build.rs` hard-fails; set `PETAL_ALLOW_NO_BACKEND=1` to build a deliberately
backend-less release, in which every join fails at runtime with a message
saying the build has no token backend). This is deliberate — a default would
mean every third-party build silently minting tokens against the maintainers'
LiveKit and Vercel accounts.

In a **debug** build, leaving it unset (or setting it explicitly empty) instead
activates the local dev token mint, which reads `LIVEKIT_URL`,
`LIVEKIT_API_KEY`, and `LIVEKIT_API_SECRET` and talks to a local
`livekit-server --dev`. That path is compiled out of release builds.

Changing the environment variable causes Cargo to rebuild the relevant crate,
so do not rely on a previously built app bundle.

### Auto-update and release signing for a fork

**A build from a plain clone never phones home.** The committed
`apps/desktop/src-tauri/tauri.conf.json` ships an EMPTY updater
(`plugins.updater.endpoints: []`, no pubkey), so an open-source build has
auto-update disabled: the app logs
`updater: no update endpoint configured in this build` once and makes no
network request. Petal's official endpoint and minisign public key live only
in `apps/desktop/src-tauri/tauri.release.conf.json`, which the official
release pipeline layers on with `tauri build --config`. The same applies to
crash reporting and analytics: `PETAL_SENTRY_DSN` / `PETAL_POSTHOG_KEY` are
baked only when explicitly set at build time and are absent by default.

If you distribute your own auto-updating builds, point the updater at YOUR
feed and key — never reuse Petal's overlay, since your users' apps would poll
Petal's update feed and replace your build with Petal's official binary. Pass
your own overlay file (`tauri build --config my.release.conf.json`) or use
Tauri's `TAURI_CONFIG` merge:

```sh
export TAURI_CONFIG='{"bundle":{"createUpdaterArtifacts":true},"plugins":{"updater":{"endpoints":["https://updates.example.com/api/updater"],"pubkey":"<your-minisign-public-key>"}}}'
```

Generate your own key with `npx tauri signer generate`; never reuse Petal's,
since you will not hold the matching private key and your updates will fail
verification.

Two related build-time values let a fork's own signed build be recognized as a
release of itself rather than an unsigned build. Unset, they fall back to
Petal's values:

| Variable | Purpose |
| --- | --- |
| `PETAL_RELEASE_BUNDLE_ID` | Bundle identifier the release-signing check expects |
| `PETAL_RELEASE_TEAM_ID` | Apple Developer Team ID in your signing certificate |

`scripts/verify-universal-app.sh` enforces that the built app carries the
expected updater anchors. Point it at yours with
`PETAL_EXPECTED_UPDATER_ENDPOINT` and `PETAL_EXPECTED_UPDATER_PUBKEY` so the
gate validates your trust anchors instead of Petal's.

Renaming and rebranding a redistributed build is also a trademark requirement —
see `TRADEMARKS.md`.

The desktop app does not need `LIVEKIT_URL`, `LIVEKIT_API_KEY`, or
`LIVEKIT_API_SECRET` for the normal join/rooms path. Those credentials belong
on the LiveKit server and backend. Keep them out of release build commands and
out of client-distributed `.env` files.

## 4. Point web-harness at the backend (optional)

If you also deploy `web-harness/`, set `VITE_PETAL_BACKEND_URL` at its build
time to the same backend origin:

```sh
cd web-harness
VITE_PETAL_BACKEND_URL=https://petal-backend.example.com npm run build
```

For Vercel, set the value in the project's build command or environment and
deploy from the `web-harness/` root. The checked-in `vercel.json` currently
contains the hosted Petal default, so replace that value for a self-hosted
deployment rather than deploying it unchanged. The harness calls
`<backend-origin>/api/token` and uses the LiveKit URL returned by that
response.

Two telemetry caveats if you deploy with the repository's own helper,
`scripts/deploy-web-harness.sh`: it **fails closed unless the built bundle
contains a Sentry ingest URL**, and it defaults `VITE_SENTRY_DSN` and
`VITE_USERDISPATCH_PUBLIC_KEY` (the feedback-form key) to Petal's own values.
A self-hosted deployment should override both — or deploy with a plain
`npm run build` and your own hosting — so it doesn't report into the
maintainers' projects. `VITE_PETAL_POSTHOG_KEY` is unset unless you set it, so
product analytics stay off by default.

For local development against a local LiveKit server, leave
`VITE_PETAL_BACKEND_URL` unset and use the harness's local token middleware;
see [`web-harness/README.md`](../web-harness/README.md).

## Smoke check

After deploying, verify that:

1. A desktop build made with your `PETAL_BACKEND_URL` can create or join a
   room.
2. The backend's `LIVEKIT_URL`, API key, and secret match the LiveKit server;
   a mismatch produces token or room-service failures.
3. A web-harness build, if deployed, has the same backend origin and is listed
   in `PETAL_ALLOWED_ORIGINS`.
4. If AI chat is enabled, `POST /api/ai-token` returns a token for a caller who
   is in a live meeting, and returns `503 AI chat is not configured` when
   `GEMINI_API_KEY` is unset.

For backend-specific endpoint and local test details, see
[`backend/README.md`](../backend/README.md). For signed desktop release
packaging, see [`RELEASING.md`](RELEASING.md).
