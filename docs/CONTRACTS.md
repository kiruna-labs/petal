# Cross-Component Contracts

These contracts must stay in lockstep across native, backend, and web-harness code. If one file changes, update the sibling files and the tests that pin the contract.

The shared fixture is `contracts/petal-contracts.json`. It is read by the
Rust contract tests (`apps/desktop/src-tauri/src/rooms.rs` and the other
modules named per section below), the browser-client test suite
(`web-harness/tests/contracts.test.ts` plus the per-topic suites —
`remoteControl`, `remoteControlGrantGate`, `displayNames`, `profileColor`,
`aiChat`, `j`), the backend tests (`backend/test/privacy.ts`,
`distribution.ts`, `hardening.ts`), and the remote-control harness preflight
(`apps/desktop/scripts/remote-control-harness-preflight.mjs`). `grep -rl
petal-contracts.json` is the authoritative reader list.

## Data-channel topics

The fixture's `topics` key pins every LiveKit data-channel topic Petal uses:
`petal.telepointer`, `petal.remote-control`,
`petal.remote-control.clipboard-text`, `petal.viewer-demand`,
`petal.pipeline-stats`, `petal.latency-probe`, `petal.draw`, `petal.ai-chat`.
Each has a section below except the two diagnostics topics, documented here:

- **`petal.latency-probe`** (`latencyProbeMessages`) — a peer-to-peer
  data-channel RTT probe for the Network Cockpit, *not* glass-to-glass video
  latency. `ping` carries `v: 1`, `kind`, `probeId`, `senderId`,
  `sendTimeMs`; the receiver echoes `pong` with the same `probeId` /
  `sendTimeMs` plus `receiverReceiveTimeMs` and `receiverSendTimeMs`, and the
  original sender computes RTT and an NTP-style clock offset on its own clock.
  Reliable delivery. Native: `apps/desktop/src-tauri/src/latency_probe.rs`;
  web: `web-harness/src/trackNames.ts` (`LatencyProbeMessage`).
- **`petal.pipeline-stats`** — cross-peer pipeline stage snapshots for the
  Network Cockpit (`apps/desktop/src-tauri/src/pipeline_stats.rs`).

## Shared-source scale metadata (`petalWindowScales`)

Alongside `petalWindowKinds`, `petalWindowTitles`, `petalWindowUrls`, and
`petalWindowZOrder`, a publisher's participant metadata carries
`petalWindowScales`: `{ "<windowId>": <capture scale> }`, the ratio between
the published pixel size and the source window's point size (e.g. `0.64` for
a downscaled capture, `1.5` for a Retina-ish one). Receivers use it to map
remote-control and telepointer coordinates back onto the source window, and
a native receiver only offers Control for a share with a positive, finite
scale entry (`0`, negative, or non-numeric values are ignored; a missing
entry means "not controllable"). The fixture's `sourceScaleMetadata` pins
the parsing vectors. Native: `transport/publisher.rs`
(`shared_window_scale_from_metadata`); web: `web-harness/src/trackNames.ts`
(`mergeSharedSourceMetadata` writes it, always `1` for canvas/display
captures).

## Test Cockpit Scenarios and Journeys

The fixture's `testCockpitScenarios` key pins the legacy cockpit scenario IDs
and tiers used by `apps/desktop/src/lib/data/testCockpit.ts`. Its
`testCockpitJourneys` key pins the feature, priority, depth, status, runnable,
and legacy metadata for the journey selector and Rust test cockpit. The Rust
`journey_contract_parity` test exists to keep that journey metadata in lockstep
with the native `JOURNEY_TABLE`; update both sides together when the cockpit
scenario or journey contract changes.

## Room Slugs and LiveKit Room Names

Canonical files:

- `apps/desktop/src-tauri/src/rooms.rs` - `slugify`, `normalize_room_credential`, `livekit_room_name`, `livekit_room_name_for`
- `backend/lib/slug.ts` - `slugify`, `normalizeRoomCredential`, `generateRoomCredential`, `livekitRoomName`
- `shared/logic/meetingCode.ts` - `slugify`, `normalizeRoomCredential`, `generateMeetingCode`, `livekitRoomName`
- `contracts/petal-contracts.json` - shared expected outputs
- `web-harness/server/tokenPlugin.ts` - dev-server token endpoint room mapping for local harness use

Slug algorithm: trim, lowercase, collapse every run of non-ASCII `[a-z0-9]` to `-`, trim leading/trailing `-`, and fall back to `room` if empty.

Expected slug outputs from the shared fixture:

| Input | Slug |
|---|---|
| `Design Review` | `design-review` |
| `design-review` | `design-review` |
| `  design   review  ` | `design-review` |
| `eng-sync` | `eng-sync` |
| `quick-mr2dzrhh` | `quick-mr2dzrhh` |
| `---` | `room` |
| `!!!` | `room` |
| `cafe creme` | `cafe-creme` |
| `Eng / Sync #2` | `eng-sync-2` |
| `UPPER lower 123` | `upper-lower-123` |

User-facing room access codes are three lowercase letter groups:
`abc-defg-hjk` (`[a-z]{3}-[a-z]{4}-[a-z]{3}`). Petal-generated codes draw from
the 24-letter alphabet `abcdefghjkmnopqrstuvwxyz` (`i` and `l` excluded as
easy to misread; `shared/logic/meetingCode.ts`); the parser still accepts
hand-typed `i`/`l`. They may be pasted with or
without hyphens and normalize to the hyphenated lowercase form. The internal
credential is never shown to users; clients derive `room-<32 lowercase hex>`
from the normalized access code and map that to `petal-room-<credential>`.
Bare labels and slug-only inputs must fail closed in room-name derivation; they
are display labels only and must not derive a guessable LiveKit room.

Expected credential vector:

| Access code | Internal credential | LiveKit room |
|---|---|---|
| `abc-defg-hjk` | `room-8535e993a1b76ed8a9ee59b265f53dfc` | `petal-room-room-8535e993a1b76ed8a9ee59b265f53dfc` |

(This is the fixture's pinned `roomCredentials` vector; the tests on every
side assert exactly these strings.)

Files to change together:

- `apps/desktop/src-tauri/src/rooms.rs`
- `backend/lib/slug.ts`
- `shared/logic/meetingCode.ts`
- `contracts/petal-contracts.json`
- `web-harness/tests/contracts.test.ts`
- Backend slug tests in `backend/test/local.ts` if the visible expected values change

## Public LiveKit Token Grants

The production backend's public `POST /api/token` endpoint accepts only
`room`, `identity`, and optional `displayName` as caller-controlled contract
fields. It always mints a visible participant token with Petal's fixed public
profile:

- `roomJoin: true`
- `canPublish: true`
- `canSubscribe: true`
- `canPublishData: true`
- `canUpdateOwnMetadata: true`
- `hidden: false`

Caller-supplied grant fields are ignored if present for backwards-compatible
JSON parsing. Hidden subscribe-only participants are reserved for trusted
server-owned paths, never the public token endpoint.

Files to change together:

- `backend/lib/handlers.ts`
- `backend/lib/livekit.ts`
- `backend/api/token.ts`
- `backend/test/privacy.ts`
- `backend/test/local.ts`
- `web-harness/src/controls.ts`
- `web-harness/tests/displayNames.test.ts`

The token response is `{ url, token, room, displayName? }`; `displayName` is
the room-level human name from LiveKit metadata, never the joining
participant's display name. Native deserializers keep it optional for older
backend responses.

## Room Directory Metadata

`POST /api/rooms` creates a named LiveKit room and stamps its metadata as
`{ displayName, open }`. Existing callers send `{ name, open? }` and receive a
fresh generated credential. Native-created rooms already have a credential, so
they send `{ name, room, open? }`, where `name` is the human display label and
`room` is the existing internal credential (`room-<32 lowercase hex chars>`).
For native credential stamps, `open` is an initial value only: if that LiveKit
room already has metadata, the backend preserves the server-side `open` value
while refreshing `displayName`. The backend must stamp that exact LiveKit room
instead of generating a new one.

The token response includes the room display name when LiveKit metadata is
available. Web clients use that server value for the meeting header and fall
back to local labels only when older or metadata-less rooms have no server
value.

Files to change together:

- `backend/lib/handlers.ts`
- `backend/lib/livekit.ts`
- `backend/api/rooms.ts`
- `backend/test/privacy.ts`
- `contracts/petal-contracts.json`
- `apps/desktop/src-tauri/src/transport/room_directory.rs`
- `apps/desktop/src-tauri/src/session/room.rs`
- `web-harness/src/roomLabels.ts`
- `web-harness/src/connection.ts`

## Closed rooms and removed participants

Room metadata (`{ displayName, open, removed? }`) is the authority for two
`POST /api/token` refusals, both `403`:

- **`open: false`** (knock-to-join). The request MUST carry `accessCode`, the
  short invite code (`abc-defg-hjk`, any casing/hyphenation), and
  `credentialForAccessCode(accessCode)` MUST equal the requested `room`
  credential. Rationale: the credential is a one-way FNV-128 hash of the code
  and is visible in LiveKit room names, JWT claims and logs, while the code
  only travels inside an invite — so a closed room demands the pre-image, not
  the hash. Open rooms ignore `accessCode` entirely, so the field is
  additive: clients omit it (never send `null`) when the code is unknown,
  which keeps open-room requests byte-identical to the pre-existing shape.
  Native sends `RoomRecord.access_code` when the local record has one; web
  sends `accessCodeForCredential(meetingCode)`, known whenever this session
  derived the credential from a join link, a typed code, or its own create.
- **`removed`** — identities an admin kicked via `POST /api/admin`
  `{ action: "kick" }`. The kick writes the identity into room metadata
  *before* `removeParticipant`, and `/api/token` refuses the identity for as
  long as the LiveKit room (hence its metadata) exists. The list is bounded
  to 64 entries, oldest dropped; an empty list is omitted from the encoded
  metadata so older rooms' metadata stays byte-identical. Every metadata
  rewrite — native re-stamps via `POST /api/rooms { name, room }` included —
  carries `removed` forward (`preservedRoomMeta`). Known limit: a kicked
  user who discards their generated identity can return under a new one;
  the kick is by identity because that is the only handle the admin has.

Availability rule: a room whose metadata cannot be read (older clients never
stamped one; the room passed its emptyTimeout; LiveKit unreachable) is
treated as open with nobody removed — the knock gate lives exactly as long as
the room does, and a creator re-stamp restores it.

Fixture: `contracts/petal-contracts.json` `closedRoomTokenRequest` (shares
the `roomCredentials` access code). Files to change together:

- `backend/lib/handlers.ts`, `backend/lib/livekit.ts`
- `backend/test/hardening.ts`, `backend/test/local.ts`
- `apps/desktop/src-tauri/src/transport/token.rs` (`BackendTokenRequest.access_code`)
- `apps/desktop/src-tauri/src/meeting_core.rs`
- `web-harness/src/controls.ts` (`tokenRequestBody`)
- `web-harness/tests/displayNames.test.ts`

## Room status (proof of possession) — replaces the public directory

There is **no public room directory.** `GET /api/rooms` returns **410 Gone**
(`{ error: 'room directory removed; use POST /api/rooms/status' }`). It used
to list every room's display name, open flag and live headcount to anyone on
the internet; the cross-machine discovery it existed for (#98/#155 — "a fresh
machine lists rooms created elsewhere") had been inert since #83 removed join
credentials from the view, and the desktop client only ever used the listing
to repair a display label. The rationale that create/list "must stay
unauthenticated because there is no credential a caller could present" is
retired: the caller's own `rooms.json` holds exactly the credentials that
prove which rooms it may ask about.

`POST /api/rooms/status`

- Request: `{ rooms: [{ room: <credential>, accessCode?: <code> }] }` —
  at most `ROOM_STATUS_MAX_ROOMS` (64) entries; more is 400. `room` is the
  internal credential (`room-<32 lowercase hex>`); `accessCode` is the
  invite's letter code.
- Response: `{ rooms: [{ id, name, open, occupancy }] }` — the same view
  shape the directory had (`id` is the opaque public id, never a credential).
- **Only rooms the caller presented are returned.** A credential that is
  malformed, unknown, or not live is silently **omitted** — never 404'd — so
  the endpoint cannot be used as an existence oracle for guessed credentials.
  An empty request returns `{ rooms: [] }` without touching LiveKit.
- A room stamped `open: false` is additionally omitted unless `accessCode`
  hashes to its credential (`credentialForAccessCode`), the same rule
  `/api/token` applies at mint. Possession of a closed room's credential
  alone reveals nothing.
- Native sends every local record's credential plus its access code when
  held (the local `open` flag is only an initial value; the server's wins).
  Web-harness does not call this endpoint.
- Status is ONE `listRooms` RPC (occupancy is its `numParticipants`, hidden
  `-gallery` bridges excluded by LiveKit) shared by every caller and cached
  per instance for `ROOMS_LIST_CACHE_MS` (3s), then filtered to the presented
  set; the 60/min per-source rooms bucket is still charged on cache hits.

`POST /api/rooms` (create/stamp) stays unauthenticated: the web join-link
flow creates rooms before any LiveKit identity exists. What bounds abuse:
- Creation has its own per-source bucket (`ROOM_CREATE_BUCKET_CAPACITY` = 20
  per 10 minutes) plus an instance-wide ceiling
  (`ROOM_CREATE_GLOBAL_CAPACITY` = 120 per 10 minutes), separate from the
  discovery bucket so a create flood cannot lock discovery and vice versa.
- All buckets live in bounded stores (`backend/lib/ratelimit.ts`: TTL = the
  limit's refill window, hard key cap with LRU eviction). They are
  per-instance; `configureRateLimitStores` is the seam for a shared store
  (Vercel KV / Redis) if global limits are ever required.

Files to change together:

- `backend/lib/handlers.ts` (`handleRoomStatus`, `ROOM_STATUS_MAX_ROOMS`)
- `backend/api/rooms/status.ts`, `backend/api/rooms.ts` (410)
- `backend/test/hardening.ts`, `backend/test/local.ts`, `backend/test/distribution.ts`
- `contracts/petal-contracts.json` (`roomStatusRequest`)
- `apps/desktop/src-tauri/src/rooms.rs` (`room_status_request`, `merge_room_status`)
- `scripts/verify-backend-live.sh`

## Gallery Bridge Token Grants (#109)

`POST /api/gallery-token` is the trusted server-owned path referenced above.
It exists so the desktop app's hidden "gallery bridge" webview participant
(a SECOND, hidden/subscribe-only LiveKit connection used only to receive
remote camera video into gallery tiles — native compositor handles only
`petal-window-*` share tracks) can get a usable token, without reopening the
public endpoint's hidden/grant clamp.

Caller-controlled fields: `room` (credential), `baseIdentity` (the caller's
OWN already-generated, visible-participant identity — no suffix), optional
`displayName`. The backend:
1. rejects `baseIdentity` if it already carries the `-gallery` suffix,
2. verifies `baseIdentity` is a CURRENT participant in that exact LiveKit room
   (via `listParticipants`) — the sole trust anchor, no shared secret,
3. derives the bridge identity itself as `<baseIdentity>-gallery`,
4. mints a fixed profile: `roomJoin: true, canPublish: false, canSubscribe:
   true, canPublishData: false, hidden: true`.

Any failure (room unresolvable, `baseIdentity` not present, already suffixed)
collapses to a single `403`/`400` so the response never leaks which case it
was.

Files to change together:

- `backend/lib/handlers.ts` (`handleGalleryToken`)
- `backend/lib/livekit.ts` (`RoomDiscoveryService`)
- `backend/api/gallery-token.ts`
- `backend/test/privacy.ts`
- `apps/desktop/src-tauri/src/transport/token.rs`
  (`fetch_gallery_access_token`, `GalleryTokenRequest`)
- `apps/desktop/src-tauri/src/gallery_bridge.rs`

## LiveKit Track Names

Microphone track:

- Format: the fixed literal `petal-mic` (no per-participant suffix — a
  participant publishes at most one mic track, and the native playback path is
  ADM-level and name-blind, so nothing needs to parse it back).
- Native constant: `apps/desktop/src-tauri/src/transport/audio.rs`
  (`MIC_TRACK_NAME`), published by `prepare_microphone` /
  `publish_prepared_microphone`.
- Native reconnect-repair consumer: `apps/desktop/src-tauri/src/session/share.rs`
  (`reconnect_publication_health`).
- Web constant: `web-harness/src/controls.ts` (`MIC_TRACK_NAME`).
- Track source: `Microphone` on both sides (`TrackSource::Microphone` /
  `Track.Source.Microphone`).

| Side | Track name | Source |
|---|---|---|
| native | `petal-mic` | `Microphone` |
| web | `petal-mic` | `Microphone` |

**Audio publish options are NOT symmetric, and that is a known open question,
not a contract.** Both sides negotiate Opus with in-band FEC (unconditional in
both SDKs), but:

| Option | Native | Web |
|---|---|---|
| RED (RFC 2198 redundant Opus) | `red: false`, explicit override — `transport/audio.rs`'s `audio_publish_options()` | not set → `livekit-client`'s `red: true` default |
| DTX | `dtx: true`, explicit | not set → SDK default (`true`) |

Both sides signal this through the same `AddTrackRequest.disable_red` field
(`vendor/livekit/src/room/participant/local_participant.rs`;
`livekit-client`'s `disableRed: !(opts.red ?? true)`), so the mechanisms match
even though the values do not. Native disables RED because of a real
browser/mobile decode-silence interop hazard (#510, fixed by PR #517); the
**reverse leg — a RED-wrapped web publisher decoded natively — has never been
validated**, and it is one of the two live hypotheses in #787. Do not "align"
the web side without evidence: `apps/desktop/vendor/webrtc-sys/libwebrtc/`
ships a prebuilt binary, so the native decode path cannot be inspected from
source, and turning RED off on the web publisher would change the one audio
direction that currently works.

Files to change together:

- `apps/desktop/src-tauri/src/transport/audio.rs`
- `web-harness/src/controls.ts`
- `contracts/petal-contracts.json` (`micTrack`)

Window shares:

- Format: `petal-window-<id>`
- Native producer/parser: `apps/desktop/src-tauri/src/transport/publisher.rs`
  - `track_name_for_window(window_id)`
  - `window_id_from_track_name(name)`
  - `window_id_for_track_name_any(name)`
- Web producer/parser: `web-harness/src/trackNames.ts`
  - `trackNameForWindow(windowId)`
  - `randomWindowId()`
- Web telepointer parser: `web-harness/src/telepointer.ts`
- Native subscriber/compositor users: `apps/desktop/src-tauri/src/transport/subscriber.rs`, `apps/desktop/src-tauri/src/compositor.rs`

Expected outputs:

| Window id | Track name |
|---:|---|
| `1` | `petal-window-1` |
| `123456` | `petal-window-123456` |
| `2147483647` | `petal-window-2147483647` |

Camera tracks:

- Format: `petal-camera-<identity-slug>`
- Native constants/parser: `apps/desktop/src-tauri/src/transport/publisher.rs`
  - `CAMERA_TRACK_PREFIX`
  - `camera_track_name(identity)`
  - `camera_window_id(track_name)`
  - `window_id_for_track_name_any(name)`
- Native camera publish path: `apps/desktop/src-tauri/src/camera_session.rs`
- Native gallery camera subscription: `apps/desktop/src-tauri/src/gallery_bridge.rs`
- Web producer: `web-harness/src/trackNames.ts`, used by `web-harness/src/main.ts`
- Native diagnostics labels: `apps/desktop/src-tauri/src/diagnostics.rs`

Expected outputs:

| Identity | Track name |
|---|---|
| `Web Tester` | `petal-camera-web-tester` |
| `___` | `petal-camera-anon` |
| `637511f2-851a-47f8-b043-823656bfc54b` | `petal-camera-637511f2-851a-47f8-b043-823656bfc54b` |

Files to change together:

- `apps/desktop/src-tauri/src/transport/publisher.rs`
- `apps/desktop/src-tauri/src/transport/subscriber.rs`
- `apps/desktop/src-tauri/src/camera_session.rs`
- `apps/desktop/src-tauri/src/gallery_bridge.rs`
- `apps/desktop/src-tauri/src/diagnostics.rs`
- `web-harness/src/trackNames.ts`
- `web-harness/src/main.ts`
- `web-harness/src/telepointer.ts`
- `web-harness/tests/contracts.test.ts`
- `contracts/petal-contracts.json`

## Data-Channel Wire Formats

### Telepointer

- Topic: `petal.telepointer`
- Native sender/receiver: `apps/desktop/src-tauri/src/telepointer.rs`
- Web constants/types: `web-harness/src/trackNames.ts`
- Web sender/rendering: `web-harness/src/main.ts`, `web-harness/src/telepointer.ts`
- Native compositor delivery: `apps/desktop/src-tauri/src/compositor.rs`

Wire JSON fields:

```json
{
  "windowId": 42,
  "userId": "web-1",
  "x": 0.5,
  "y": 0.25,
  "visible": true,
  "activity": "click"
}
```

`activity` is optional. Current values are `click` and `type`. Coordinates are
normalized `0..1` within the source window/media. Native clients may publish the
same `windowId` from either the source sharer's local window frame or a viewer's
received compositor content frame; `windowId` remains the original shared-window
identifier so every participant renders the pointer on the same shared surface.

### Identity Palette

Draw ink, telepointers, remote-window headers, and active share controls use
the shared six-color identity palette pinned in `contracts/petal-contracts.json`
as `identityPalette`. New clients publish the user's selected palette index in
participant metadata as `petalIdentityPaletteIndex`; older clients omit it and
receivers fall back to the desktop hash (`hash = hash * 31 + utf16_code_unit`
modulo six).

`petalIdentityPaletteIndex` is visual preference only. It is self-set
participant metadata and is not a security boundary. Receivers still derive
draw and telepointer attribution from the authenticated LiveKit participant
identity, and draw/telepointer payloads never carry or trust a color field.
Metadata writers must merge this key with existing participant metadata such as
`petalWindowKinds`, `petalWindowTitles`, and `petalWindowColorProfiles`.

Files to change together:

- `apps/desktop/src-tauri/src/telepointer.rs`
- `apps/desktop/src-tauri/src/compositor.rs`
- `web-harness/src/trackNames.ts`
- `web-harness/src/telepointer.ts`
- `web-harness/tests/profileColor.test.ts`
- `web-harness/src/main.ts`
- `web-harness/tests/contracts.test.ts`
- `contracts/petal-contracts.json`

### Window Z-Order

The sharer's currently-shared window ids, front-to-back (index 0 =
frontmost), published in participant metadata as `petalWindowZOrder`
(#875). Native producer: `apps/desktop/src-tauri/src/telepointer.rs`'s
sender loop, sourced from `window_registry::Snapshot.order` (the
front-to-back CG list order) filtered to the currently-shared window ids,
piggybacking the same ~9Hz registry read the loop already does for
`SessionState::update_share_frames_and_visibility` (issue #30). It
republishes only when the shared subset's own front-to-back order actually
changed -- `transport::publisher::RoomConnection::set_shared_window_order`
stages the comparison internally
(`transport::publisher::stage_shared_window_order`) so an unrelated
reshuffle of unshared windows elsewhere on screen, or a repeated identical
poll, never triggers a `set_metadata` round trip.

New clients publish this key; older clients omit it entirely, and
receivers must treat "key absent" and "malformed value" identically as "no
rank data" -- never as an empty order. An explicitly-published empty array
(nothing currently shared) is valid and distinct from an absent key.

Native reader: `transport::publisher::shared_window_z_order_from_metadata`
(whole order) and `shared_window_z_rank_from_metadata` (one window's
front-to-back rank). The native receiver stores the decoded rank per
`(owner_identity, window_id)` on `CompositorWindow.z_rank` via
`compositor::update_window_z_rank`, called from the
`RoomEvent::ParticipantMetadataChanged` handler in
`transport/subscriber.rs` alongside the existing per-window metadata
refresh. Web reader: `web-harness/src/trackNames.ts`'s
`sharedWindowZOrderFromMetadata` / `sharedWindowZRankFromMetadata`.

Like `petalIdentityPaletteIndex`, this is visual/ordering preference only
carried in self-set participant metadata, not a security boundary. Writers
must merge this key non-destructively with the rest of `ShareMetadata`
(`petalWindowKinds`, `petalWindowTitles`, `petalWindowColorProfiles`,
`petalIdentityPaletteIndex`, etc.) -- `encode_window_metadata` builds one
JSON object from all of them together, so publishing a new z-order can
never clobber an unrelated key.

Files to change together:

- `apps/desktop/src-tauri/src/transport/publisher.rs`
- `apps/desktop/src-tauri/src/transport/subscriber.rs`
- `apps/desktop/src-tauri/src/compositor.rs`
- `apps/desktop/src-tauri/src/telepointer.rs`
- `web-harness/src/trackNames.ts`
- `web-harness/tests/contracts.test.ts`
- `contracts/petal-contracts.json` (`windowZOrderMetadata`)

### Shared Window URLs

The sharer's best-known address for a shared browser window, published in
participant metadata as `petalWindowUrls`: a JSON object mapping the
stringified `windowId` to a **privacy-minimized http(s) URL** (#915). Value
shape:

```json
{ "petalWindowUrls": { "123": "https://example.com/docs" } }
```

Privacy rule, enforced on write (native macOS extraction,
`apps/desktop/src-tauri/src/browser_url.rs`'s `privacy_minimized_openable_url`)
and re-checked on every read: the query string and fragment are always
stripped (`?...`/`#...` truncated at the first occurrence), and a value that
does not start with `http://` or `https://` is dropped entirely rather than
published or parsed -- a raw `file://`/`about:`/empty value never reaches a
receiver as a candidate. `petalWindowUrls` is only ever populated by the
macOS sharer (Automation/`osascript` extraction, gated on the
`com.apple.security.automation.apple-events` entitlement); a Windows or
web-harness sharer omits the key entirely, and receivers must treat "key
absent" and "url for this window absent" identically as "no Open URL
button," never as an error.

Native reader: `transport::publisher::shared_window_url_from_metadata`. Web
reader: `web-harness/src/trackNames.ts`'s `sharedWindowUrlFromMetadata`. Both
are pinned against the SAME fixture entry,
`contracts/petal-contracts.json`'s `windowUrlMetadata`, which carries a plain
already-minimized URL, a URL that still needs `?token=x#frag` stripped, and a
non-http(s) URL that must parse to `None`/`null` -- so a minimization-rule
drift between native and web fails on both sides, not just one.

Like `petalWindowZOrder`, this is self-set participant metadata merged
non-destructively into `ShareMetadata` alongside `petalWindowKinds`,
`petalWindowTitles`, `petalWindowColorProfiles`, etc.; it carries no
authentication weight and a receiver does not need to trust the sharer's
minimization beyond privacy-by-convention (the sharer's own client is the one
doing the stripping, not a hostile boundary).

Files to change together:

- `apps/desktop/src-tauri/src/browser_url.rs`
- `apps/desktop/src-tauri/src/transport/publisher.rs`
- `web-harness/src/trackNames.ts`
- `web-harness/tests/contracts.test.ts`
- `contracts/petal-contracts.json` (`windowUrlMetadata`)

### Draw Annotations

- Topic: `petal.draw`
- Version: `v: 1`
- Web constants/types/parser: `web-harness/src/trackNames.ts`, `web-harness/src/draw.ts`
- Delivery: reliable LiveKit data-channel messages. `points` messages batch
  normalized points instead of sending one packet per sample.

Wire JSON fields common to every variant:

- `v`
- `type`
- `ownerIdentity`
- `windowId`
- `seq`
- `strokeId`
- `points`

`ownerIdentity` is the LiveKit participant identity of the user who owns the
target shared window. It scopes `windowId`, which is only unique per owner.
The drawer identity/color is derived receiver-side from the authenticated
data-channel sender identity and is never trusted from the payload. `seq` is
monotonic per drawer + `ownerIdentity` + `windowId`. `points` are normalized
source-window coordinates, each shaped as `{ "x": number, "y": number }` with
both values in `0..1`.

Camera-tile drawing uses the same payload shape. The `windowId` is the
high-bit synthetic camera id derived from the full `petal-camera-<slug>` track
name by FNV-1a u32 plus `| 0x80000000` (`camera_window_id` in native,
`cameraWindowId` in web). Web camera tiles expose that id only as
`data-draw-window-id`; they must not set `data-window-id`, because that
attribute is reserved for real shared windows used by remote-control,
telepointer, viewer-demand, and remote-window chrome.

As of #277, web clients render camera-tile draw annotations and native validates
and quietly ignores those high-bit camera payloads instead of treating them as
missing remote windows. Native desktop camera-tile rendering is a separate
follow-up because desktop Gallery camera tiles are not compositor/pointer-overlay
windows.

Kinds:

- `begin`: starts a stroke. `strokeId` is a non-empty sender-scoped id and
  `points` contains the first one or more normalized points.
- `points`: appends one or more normalized points to an existing stroke.
- `end`: completes a stroke. `points` may be empty or contain final normalized
  points.
- `clear`: clears drawing state for the sender/window. `strokeId` is `null` and
  `points` is empty.
- `text`: places a text annotation. Carries one extra field, `text` (the
  annotation string), plus a single anchor point in `points`; pinned by the
  fixture's `drawMessages` `text` vector (`"Hello Petal"`). Implemented by
  `draw.rs` natively and `web-harness/src/draw.ts` (`DRAW_TYPES`) on the web.

Files to change together:

- `apps/desktop/src-tauri/src/draw.rs`
- `apps/desktop/src-tauri/src/transport/publisher.rs`
- `apps/desktop/src/routes/compositor/control/+page.svelte`
- `apps/desktop/src/routes/compositor/pointer/+page.svelte`
- `web-harness/src/trackNames.ts`
- `web-harness/src/draw.ts`
- `web-harness/src/drawSender.ts`
- `web-harness/src/drawDisplay.ts`
- `web-harness/tests/contracts.test.ts`
- `web-harness/tests/drawSender.test.ts`
- `web-harness/tests/drawDisplay.test.ts`
- `contracts/petal-contracts.json`

### AI Chat

Topic `petal.ai-chat`, version 1, **reliable** (session state and transcript
lines must not be dropped; lossy is reserved for continuous streams like
pointer moves). Every message is keyed by `{windowId, ownerIdentity}` — a raw
`CGWindowID` is only unique on the machine that produced it.

The Gemini session always runs on the **sharer's** machine: only they have the
window's pixels and its accessibility tree, and only they can (in #658) act on
it. Other participants drive and observe it through this topic.

| `type` | Direction | Payload |
|---|---|---|
| `startRequest` / `stopRequest` | any participant → owner | — |
| `state` | owner → all | `active`, `startedBy?`, `secondsLeft?`, `activeSpeaker?`, `error?` |
| `pttStart` / `pttEnd` | speaker → all | — |
| `sendText` | any participant → owner | `text` |
| `transcript` | owner → all | `role` (`user`\|`assistant`), `text`, `final` |

#### Authorization is per message kind

Sender identity comes from the authenticated LiveKit participant, never the
payload — the same invariant as telepointer and draw. That prevents forged
*attribution*, but not forged *authority*, so each kind additionally restricts
who may send it:

- `startRequest` / `stopRequest` — **any current participant**. The owner
  decides whether to act and enforces its own preconditions (feature enabled,
  window actually shared).
- `state`, `transcript` — **the window owner only**. Otherwise a peer could
  announce a session that is not running, or put words in the assistant's
  mouth on someone else's window.
- `pttStart` / `pttEnd` — **for the sender themselves only**. A peer must not
  be able to make the host tap another participant's microphone.
- `sendText` — **any current participant**, same shape as start/stop. Unlike
  PTT it never claims the floor — a typed message has no "who's speaking"
  ambiguity to arbitrate, so any number of participants may each send one
  independently. The owner still refuses it while the PTT floor is held: a
  `clientContent` turn overlapping an open `realtimeInput` activity window is
  undefined by the Live API.

Enforced by `ai_chat::wire::authorize`, which is pure and unit-tested against
the fixtures because it is the whole security boundary of this topic.

#### Floor control and liveness

Exactly one participant holds the push-to-talk floor at a time: Gemini's
manual-activity mode is a single serial audio stream, so two speakers
interleaved corrupt the turn rather than mixing. The owner reports the holder
in `state.activeSpeaker`, and answers a losing claimant with
`error: "busy"`.

`state` doubles as a heartbeat, republished every 5s while live. A receiver
that misses 3 consecutive heartbeats — or sees the owner disconnect — declares
the session gone and clears its UI, so a crashed host cannot leave the room
showing a phantom live assistant.

#### Assistant audio

The assistant's voice is published by the owner as a normal LiveKit audio
track named `petal-ai-window-<windowId>`. Every surface must exclude
`petal-ai-*` tracks from speaking-indicator and microphone-mute logic: the
assistant is not the sharer, and muting your own microphone must not mute it.
Any parser that pattern-matches track-name prefixes has to classify this
namespace explicitly rather than falling through to "unknown".

**One place that exclusion structurally cannot reach (#659, resolved):**
LiveKit's `ActiveSpeakersChanged` is per-*participant*, computed from
aggregate audio energy across every track that identity publishes — mic and
`petal-ai-*` combined, since both are published under the sharer's own
identity. Track-name filtering (above) has nothing to filter at that layer.
Resolution: a muted mic transmits zero energy, so any "speaking" attributed
to a **muted** identity cannot be their own voice — both clients now skip
`ActiveSpeakersChanged` for a muted participant (`apps/desktop/src-tauri/src/
presence.rs`'s `apply_speaking`; `web-harness/src/connection.ts`'s
`ActiveSpeakersChanged` handler, via `Participant.isMicrophoneEnabled`). This
closes the common case (sharer muted while listening to an answer) without a
new wire signal. It does **not** close an unmuted-but-silent sharer showing
as speaking while the assistant answers — that needs real per-track
audio-level detection, out of scope for this fix.

#### `error` vocabulary (closed set)

`stopped`, `time-limit`, `disabled`, `not-shared`, `busy`, `rate-limited`,
`hosted-unavailable`, `offline`, `mint-failed`, `model-unavailable`, `quota`,
`error`. Freeform strings are not permitted — every surface renders copy from
these tokens, so a new one must be added to the contract and both
implementations together.

Pinned by:

- `apps/desktop/src-tauri/src/ai_chat/wire.rs` (+ its contract tests)
- `contracts/petal-contracts.json` (`topics.aiChat`, `aiTracks`,
  `aiChatMessages`, `aiChatEndReasons`)

### Remote Control

- Topic: `petal.remote-control`
- Version: `v: 1`
- Native portable protocol/session module: `apps/desktop/src-tauri/src/remote_control_core.rs`
- Native macOS adapter/orchestration: `apps/desktop/src-tauri/src/remote_control.rs`
- Native overlay integration: `apps/desktop/src-tauri/src/compositor.rs`
- Web constants/types/helpers: `web-harness/src/trackNames.ts`, `web-harness/src/remoteControl.ts`
- Web sender: `web-harness/src/main.ts`
- Live scenario: `apps/desktop/scripts/remote-control-scenario.mjs`

Common fields:

- `v`
- `kind`
- `targetUserId`
- `controllerId`
- `windowId`
- `seq`

`windowId` remains the sharer's numeric source-window id on the data-channel
wire. Native viewer-side compositor and control-overlay IPC additionally scope
that id by the LiveKit owner identity so two different sharers with the same
numeric OS window id do not collide locally; this does not change the v1 remote
control packet shape.

Kinds and additive capability envelope:

- `request` and `release`: the legacy shape remains common fields only.
- `pointer`: adds `action` (`move`, `down`, `up`, or atomic `click`), `x`, `y`,
  `button`, `buttons`, `clickCount` (multi-click; the fixture's
  `pointer-double-click` vector pins it), and `modifiers`.
- `wheel`: adds `x`, `y`, `deltaX`, `deltaY`, `deltaMode`, and `modifiers`.
- `key`: adds `action`, `key`, `code`, optional `location`, `repeat`, and
  `modifiers`.
- `text`: adds `text` and `modifiers`. This remains the generic text/IME input
  path used by the harness and by composed text from the controller UI. Native
  clipboard Paste does not use this kind; it uses the native-only clipboard
  stream described below.
- `status`: host-to-controller grant/policy state.
- `result`: the terminal disposition of one reliable discrete operation,
  carried in the `outcome` field. `applied` means an observed semantic target
  operation; `submitted` means the host submitted input to the OS but did not
  observe the target application's effect. A successful disposition cannot
  carry `failureCode`. (Fixture vector: `result-applied-v2`.)

`modifiers` is `{ "alt": boolean, "ctrl": boolean, "meta": boolean, "shift": boolean }`.

The capability envelope is additive. Its optional fields are:

- `targetKind`: `window` or `display`; omitted means the legacy window target.
- `shareInstanceId`: opaque identity of one live share/publication instance.
- `controllerCapabilities` on `request`; `hostCapabilities` on an accepted
  `status`. Known values are `legacyControl`, `discretePointerV1`,
  `discreteScrollV1`, `windowLocalPointer`, `globalKeyboard`, `uiaInvoke`,
  `uiaScroll`, and `unicodeText`.
- `reason`: additive status/request metadata. `controllerUpgradeRequired`
  rides the stable `requestUnavailable` status when a capable Windows host
  receives a legacy controller request; `requestEscalation` rides a `request`
  when a controller asks the sharer for full control of a cursor-preserving
  share (host-side approval, never auto-escalated); `consentDenied` and
  `consentTimedOut` ride the `denied` status (below).
- `controlSessionId`, `inputId`, `inputSeq`,
  `operationFingerprintVersion`, and `operationFingerprint` bind one reliable
  discrete operation to one grant.
- `deliveryRoute` and privacy-safe `failureCode` describe a terminal `result`.

Unknown future values in optional `targetKind`, capability, reason,
`deliveryRoute`, or `failureCode` metadata are ignored rather than making an
otherwise known packet unparsable. Unknown packet `kind`, required `action`,
or required `status` values still reject that packet. A host MUST NOT infer
`display` from an unknown or partial envelope. For a capable operation,
`targetKind` and non-empty `shareInstanceId` are both required and are part of
the grant identity; stale share instances and target-kind mismatches fail
authorization.

Legacy packets omit every new field and retain their old JSON and binary
representations. A legacy Mac host/controller keeps the existing flow. A
capable controller can negotiate the new envelope with a capable Windows host.
A Mac controller may use that envelope against Windows while the same build
continues to advertise legacy host behavior when sharing from Mac. UI policy
availability is derived from the remote host's advertised capabilities, never
from the controller platform alone.

The canonical operation fingerprint keeps the existing v1 bytes unchanged for
legacy operations. If `targetKind` or `shareInstanceId` is present, it appends
the tag byte `2`, the target-kind byte (`1` window, `2` display), and the
optional length-prefixed UTF-8 share-instance id before SHA-256. The capable
vector is pinned as `pointer-click-capable-window` in
`contracts/petal-contracts.json` and asserted in Rust and TypeScript.

A Windows `active` grant is session state; `status`/`result` operation feedback
is not. Once active, `targetPaused`, `targetUnavailable`, `requestFailed`,
`accessibilityDenied`, `notForeground`, `occluded`, `integrityBlocked`,
`secureField`, `unsupportedRoute`, `staleShareInstance`, `injectionTimeout`,
and unknown future feedback MUST NOT remove the grant, clear its operation
correlation, or disable the control surface. Controllers render feedback
briefly and keep forwarding. `stopped` and `disabled` are lifecycle statuses;
share removal/replacement, release, disconnect, and room teardown terminate
through their lifecycle paths. Before any grant exists, request failure remains
inactive and cannot fabricate authorization.

A reliable capable operation uses its correlated `result` only. A legacy-shaped
high-rate operation has no result identity, so the host MAY send a throttled,
privacy-safe feedback `status`; it has the same non-mutating semantics. No
failure packet authorizes fallback to another injection route.

Transient feedback statuses (`occluded`, `integrityBlocked`, `secureField`,
`notForeground`, …) re-emit on every occurrence — they bypass the persistent
lifecycle latch (`active`/`stopped`/`disabled`), so an identical refusal after
the controller's ~3-second warning clears is rendered again without mutating
the grant. The 1-second host-side replay-failure throttle still bounds the
sender; the controller overlay replaces one pointer-transparent banner rather
than stacking toasts.

#### Sharer consent (`awaitingConsent` / `denied`)

The host's remote-control policy is host-side authority, never on the wire:
`off` (every request -> `disabled`), `ask` (the default), or `auto` (the
pre-consent behaviour: an authenticated in-room requester is granted
immediately). Under `ask` the host PARKS an authenticated request and
prompts the sharer:

- `status: "awaitingConsent"` -- host-to-controller, sent immediately when a
  request is parked and re-sent on a repeat request while still parked. It
  is non-lifecycle feedback: it carries no `grantToken`, installs nothing,
  and MUST NOT remove an existing grant. Controllers render it as a neutral
  "Waiting for approval" and extend their own request timeout to cover the
  sharer's 30-second window (the 8-second no-answer timeout would otherwise
  fire first).
- `status: "denied"` with `reason: "consentDenied"` (explicit Deny, share
  stopped, requester left, policy turned off) or `reason: "consentTimedOut"`
  (no answer within 30 s) -- host-to-controller, non-lifecycle. A timeout
  NEVER grants. Controllers render it as a warning ("Control denied") and
  leave request state.
- Allow runs the exact `auto` authorize tail: the gate is re-checked at
  answer time (policy still allows, requester still present), the grant
  token is minted only then, and the targeted `active` status follows.
- Ordinary Allow/Deny is host-local (`remote_control_answer_consent`); a Windows
  full-control escalation uses `remote_control_answer_escalation`. Neither is
  a wire message, and both revalidate before changing host state.
- A re-request from a controller that already holds a grant is idempotent
  and answered with `active`, never re-prompted.

Pre-consent peers: a host that does not know `awaitingConsent`/`denied`
drops those packets (unknown required `status`), so an old controller sees
nothing until `active` arrives or its own timeout fires -- acceptable
degradation, no fabricated authorization. Fixture vectors:
`status-awaiting-consent`, `status-denied`.

Control-mode negotiation is host-authoritative. The supported modes are
cursor-preserving window-local control and explicit sharer-approved full-pointer
control. A controller that wants a stronger mode sends a new request and waits
for sharer approval; an individual operation never escalates itself. Window-share wheel is a
cursor-preserving `WM_MOUSEWHEEL`/`WM_MOUSEHWHEEL` route delivered via
`SendMessageTimeoutW` (synchronous, `SMTO_ABORTIFHUNG` + 250ms — the same
mechanism Chromium uses to redirect wheel between its own windows) whose
destination is the shared window's own SCROLLABLE descendant under the cursor
(`EnumChildWindows` + `GetScrollInfo`/`WS_*SCROLL`), with the top-level window
as fallback — this lands the wheel on the actual editor/render widget for both
Chromium apps (browser and Win11 Notepad) — covering windows neither block nor
redirect it, because the message is addressed to the shared window by ID — no
focus, no `SetCursorPos`, no `SendInput`, no fallback; a successful delivery
reports `submitted`, never `applied`. (Chromium-specific: at a point
physically covered on the sharer's desktop, the browser still ignores the
wheel because Chromium reroutes wheel input to the window under the pointer;
non-Chromium apps such as Win11 Notepad scroll even when covered.)
Display-share wheel keeps the serialized global `SetCursorPos` + marked
`SendInput` route, scoped to a point inside the shared display; the remaining
window-pointer/key/text routes also keep their global marked `SendInput` path,
and occlusion/covered checks apply only to those global-cursor/foreground
routes, never to ID-addressed wheel.

`status` additionally carries `supportsBinaryHotPath:boolean` for the legacy
pointer-move/wheel optimization (#370). It is set `true` when a supporting host
emits `status: "active"`; absent/false means the controller must keep sending
JSON for that session.

#### Native clipboard extension

Native desktop clients add one plain-text clipboard extension without changing
`RemoteControlMessage` or adding capability/version negotiation:

- A reliable `kind: "copy"` request remains on `petal.remote-control`. It carries
  the common target/controller/window fields, the active `grantToken`, and a
  32-lowercase-hex-character `operationId`. A capable controller may also
  carry the existing `targetKind: "window"` and `shareInstanceId` envelope so
  a Windows host can bind the request to the exact live share; the pair is
  omitted for legacy Mac-host requests. Older clients and browser peers do not
  recognize this kind and ignore it.
- The response, and the independent remote Paste command, use targeted reliable
  LiveKit byte streams on `petal.remote-control.clipboard-text` with MIME
  `text/plain; charset=utf-8`. Stream attributes are exactly `operationId`,
  `direction` (`copyResponse` or `paste`), `windowId`, and `grantToken`.
- A stream declares and delivers 1 through 1,048,576 bytes exactly. The bytes
  must be nonempty valid UTF-8 plain text with no NUL. Recognized OS file-list,
  file-promise, and other actual file clipboard formats are rejected before any
  companion path text is read; path-looking text copied as ordinary text is
  allowed. Text is never truncated.
- A Copy succeeds only by delivering its targeted response stream. Paste is
  one-way and has no success or failure acknowledgement, result packet, retry,
  queue, restoration, or retained transaction. Existing remote-control grant,
  sender, target-owner, application-window, and room-generation checks remain
  mandatory.
- Native keyboard shortcuts have fixed boundary semantics: Copy means B→A and
  Paste means A→B. They do not infer or pair a B-local workflow. Generic
  `kind: "text"` remains for IME/composed input and existing harness behavior;
  it is not the native clipboard Paste route.

The machine-readable contract entries are `remoteClipboardMessages`,
`remoteClipboardStreams`, `topics.remoteClipboardText`, and the added
`copyRequest`/`clipboardTextStream` policy entries in
`contracts/petal-contracts.json`.

Transport policy is authoritative in the contract fixture:

| Packet | Reliability | Destination | Authority |
|---|---|---|---|
| `request`, `release` | reliable | host | authenticated controller |
| `status`, `result` | reliable | controller | authenticated host |
| pointer move with no held buttons | lossy | host | authenticated controller |
| held-pointer or discrete pointer | reliable | host | authenticated controller |
| legacy wheel | lossy | host | authenticated controller |
| discrete scroll, key, text | reliable | host | authenticated controller |
| `copyRequest` | reliable | host | authenticated controller |
| `clipboardTextStream` | reliable | one targeted native participant | active remote-control grant |

The exact machine-readable matrix is
`contracts/petal-contracts.json.remoteControlPacketPolicy`.

#### Binary hot-path frame (pointer-move / wheel only)

To avoid a full JSON packet per legacy pointer-move/wheel event, `pointer`+
`move` and legacy `wheel` messages MAY be sent as a fixed 27-byte little-endian
binary frame instead of JSON (distinguished on the wire by the first byte: JSON
always starts `0x7B` / `{`, the binary frame starts `BINARY_MAGIC = 0x50`). A
v2 discrete wheel MUST stay JSON: the fixed frame cannot carry its admission
fields, operation fingerprint, or correlated-result identity. Layout:

| Bytes | Field | Notes |
|---|---|---|
| 0 | magic | `0x50` |
| 1 | version | `1` |
| 2 | kind | `4` = pointer move, `5` = wheel |
| 3 | action | `1` = move (pointer only), `0` otherwise |
| 4-7 | seq | `u32` LE |
| 8-11 | windowId | `u32` LE |
| 12-13 | x | `u16` LE, fixed-point `0..0xffff` over `0.0..1.0` |
| 14-15 | y | `u16` LE, fixed-point `0..0xffff` over `0.0..1.0` |
| 16 | buttons | `u8` (pointer only; `0` for wheel) |
| 17 | modifiers | bitfield: `alt=1, ctrl=2, meta=4, shift=8` |
| 18-19 | deltaX | `i16` LE (wheel only; `0` for pointer) |
| 20-21 | deltaY | `i16` LE (wheel only; `0` for pointer) |
| 22 | deltaMode | `u8` (wheel only; `0` for pointer) |
| 23-26 | tokenFingerprint | `u32` LE, `fnv1a32(utf8(grantToken))` |

`tokenFingerprint` (#370 corrective pass) exists because the frame has no room
for the real grant-token string. The sender fingerprints its live grant token
with `fnv1a32` (Rust: `remote_control_core.rs::fnv1a32`; TS:
`remoteControl.ts::fnv1a32` -- FNV-1a, 32-bit, offset basis `0x811c9dc5`, prime
`0x01000193`; pinned vectors incl. `fnv1a32("") = 0x811c9dc5`,
`fnv1a32("a") = 0xe40c292c` live in `contracts/petal-contracts.json`'s
`fnv1a32TestVectors` and are asserted from both languages' test suites). A
sender with no grant token to fingerprint MUST NOT emit a binary frame at all
(fall back to JSON, which still carries a real `grantToken` field). The Rust
receiver (`remote_control.rs::message_from_binary`) asks
`RemoteControlEngine` for the CURRENT legacy grant token for the frame's
`(windowId, controllerId)`, fingerprints
it the same way, and rejects the whole packet (never falls into the
tokenless-compatibility window meant for old JSON clients) on any mismatch or
missing session. Byte-level fixtures for both frame kinds are pinned in
`contracts/petal-contracts.json`'s `remoteControlBinaryFrames`.

Binary frames carry no `targetUserId` field at all -- LiveKit delivery
targeting (`destinationIdentities`) is the only thing that scopes them to the
intended host. Both `apps/desktop/src-tauri/src/remote_control.rs::publish_message`
(native) and `web-harness/src/remoteControl.ts::remoteControlPublishOptions`
(web) MUST set `destinationIdentities` to the message's `targetUserId` for
every remote-control publish, not just the binary ones.

Files to change together:

- `apps/desktop/src-tauri/src/remote_control.rs`
- `apps/desktop/src-tauri/src/remote_control_core.rs`
- `apps/desktop/src-tauri/src/compositor.rs`
- `apps/desktop/src-tauri/src/remote_control_core.rs` (`RemoteControlPolicy`, reasons)
- `apps/desktop/src-tauri/src/control_consent.rs` + `apps/desktop/src/routes/control-consent/`
- `apps/desktop/src/lib/remoteControlFeedback.ts`
- `apps/desktop/src/lib/ipc.ts`
- `web-harness/src/trackNames.ts`
- `web-harness/src/remoteControl.ts`
- `web-harness/src/remoteControlUi.ts`
- `web-harness/src/harnessApi.ts`
- `web-harness/src/context.ts`
- `web-harness/src/main.ts`
- `web-harness/tests/contracts.test.ts`
- `apps/desktop/scripts/remote-control-scenario.mjs`
- `contracts/petal-contracts.json` if pinned field vectors change

### Viewer Demand

- Topic: `petal.viewer-demand`
- Native sender/receiver: `apps/desktop/src-tauri/src/viewer_demand.rs`
- Native quality policy: `apps/desktop/src-tauri/src/session/share.rs`
- Web constants/types: `web-harness/src/trackNames.ts`

Receivers publish viewer demand while a remote compositor window is open and
visible. Sharers use that demand to keep the matching shared window at Full
quality even when it is not the sharer's most recently toggled share. Demand is
passive: no backend change and no per-track ACL change.

Wire JSON fields:

```json
{
  "v": 2,
  "kind": "heartbeat",
  "targetUserId": "native-1",
  "viewerId": "web-1",
  "windowId": 42,
  "seq": 9,
  "visible": true,
  "width": 1280,
  "height": 720,
  "scale": 2,
  "pixelWidth": 2560,
  "pixelHeight": 1440
}
```

Kinds are `open`, `closed`, and `heartbeat`. Receivers publish `closed` or a
message with `visible: false` when the remote window is hidden/retired. Sharers
also expire stale demand after missed heartbeats. Version 2 dimensions are the
visible video content box: `width`/`height` are logical/CSS points, `scale` is
the receiver backing/device scale, and `pixelWidth`/`pixelHeight` are the
derived physical-pixel demand. Version-1 messages without the new fields remain
valid and are interpreted as 1x.

Files to change together:

- `apps/desktop/src-tauri/src/viewer_demand.rs`
- `apps/desktop/src-tauri/src/session/share.rs`
- `apps/desktop/src-tauri/src/compositor.rs`
- `web-harness/src/trackNames.ts`
- `web-harness/tests/contracts.test.ts`
- `contracts/petal-contracts.json`

### Pipeline Stats

- Topic: `petal.pipeline-stats`
- Version: `v: 1`
- Delivery: reliable LiveKit data-channel messages at low rate, currently
  matching the native diagnostics poll cadence.
- Native sender/receiver: `apps/desktop/src-tauri/src/pipeline_stats.rs`
- Native model merge: `apps/desktop/src-tauri/src/diagnostics.rs`
- Web constants/types: `web-harness/src/trackNames.ts`

Each peer reports only the pipeline stages it measured locally. A shared-window
owner publishes sender-side grab/encode stages to receivers. A receiver
publishes receive/decode stages back to the owner. `ownerIdentity` scopes
`windowId`; the authenticated LiveKit data-channel sender is the trusted
reporter, and receivers must not trust `reporterId` if it disagrees.

Wire JSON fields:

```json
{
  "v": 1,
  "role": "sender",
  "reporterId": "native-1",
  "ownerIdentity": "native-1",
  "windowId": 42,
  "seq": 7,
  "sentAtMs": 1720000000123,
  "grabbed": { "width": 1280, "height": 720, "fps": 30, "kbps": null },
  "encodedSent": { "width": 1280, "height": 720, "fps": 29.5, "kbps": 1800 },
  "received": null,
  "decoded": null,
  "captureState": {
    "state": "live",
    "fps": 30,
    "dirtyRectCount": 2,
    "dirtyAreaPx": 184320,
    "occlusionPct": 0,
    "cpu": {
      "lockCopyMs": 0.74,
      "convertMs": 1.32,
      "captureFrameReturnMs": 0.21
    }
  },
  "receiverFreeze": null,
  "publicationSid": "TR_publication",
  "shareEpoch": "e1",
  "lifecycle": "published"
}
```

`role` is `sender` or `receiver`. Stage fields are nullable; absent stages are
`null`, never zero-filled. `sentAtMs` is the reporter's wall-clock epoch
milliseconds and is informational only; local receipt time determines staleness.
Sender messages carry `captureState` when available: `state` is `live`,
`idle`, `occluded`, or `wedged`; `occlusionPct` is a percentage; `cpu` reports
send-side SCK lock/copy, BGRA-to-I420 conversion, and
`NativeVideoSource::capture_frame` return timing in milliseconds. Receiver
messages carry `receiverFreeze` when available: `freezeCount`, `framesDropped`,
and nullable `qualityLimitationReason`.

`publicationSid`, `shareEpoch`, and `lifecycle` are additive optional v1
fields. `shareEpoch` is opaque and generated by the owner for each share
attempt; an early receiver lifecycle report may omit it and is correlated by
the shared publication SID until owner epoch evidence arrives. Lifecycle values
are `captureReady`, `published`, `subscribed`, `firstDecoded`,
`firstPresented`, `unsubscribed`, `unpublished`, and `terminalFailure`.
Only `unpublished` and `terminalFailure` are terminal; `unsubscribed` is a
recoverable observation and must permit a later re-subscribe/presentation.
Receivers send lifecycle reports directly to the owner. Reducers use local
receipt order and `(owner, window, reporter, publicationSid, shareEpoch)` to
ignore duplicates and must never let a terminal epoch erase a successor.

Files to change together:

- `apps/desktop/src-tauri/src/pipeline_stats.rs`
- `apps/desktop/src-tauri/src/diagnostics.rs`
- `apps/desktop/src/lib/ipc.ts`
- `apps/desktop/src/lib/data/networkCockpit.ts`
- `web-harness/src/trackNames.ts`
- `web-harness/src/pipelineStats.ts`
- `web-harness/tests/contracts.test.ts`
- `contracts/petal-contracts.json`

## Invite Links and Join Vectors

Canonical HTTPS invite links:

- Format: `https://<domain>/<name-or-label>/<access-code>` or
  `https://<domain>/<access-code>` when no display name is set.
- Backend interstitial: `web-harness/api/j.ts` (moved from `backend/api/j.ts`
  in the 2026-07-08 domain split — see `CLAUDE.md`'s "Deployment & domains"
  table), reached by the Vercel rewrite
  `/:label/:code -> /api/j?label=:label&code=:code` or
  `/:code -> /api/j?code=:code`.
- The `<name-or-label>` path segment is cosmetic only. Parsers may display it,
  but must ignore it for authorization and room selection.
- The short access code is the only user-facing join material. A label, name,
  slug, cosmetic path segment, or old `<label>-<32hex>` credential is never
  sufficient to join.
- Desktop and web-harness invite-copy controls emit this HTTPS shape for access
  codes and show the exact copied URL near the copy action.
- Opening the HTTPS route returns a small interstitial that attempts
  `petal://join/<access-code>` on load, keeps an explicit Open Petal link for
  browsers that require a user gesture, offers `/api/download`, and offers a
  browser join URL carrying `?code=<access-code>`.
- Browser join target is configured with `PETAL_WEB_JOIN_URL`, treated as an
  origin/base only: any configured path is discarded before adding `?code=`.
  The default is the production browser client, `https://meet.petal.live`
  (`DEFAULT_WEB_JOIN_BASE_URL` in `web-harness/api/j.ts`).

Native deep link (accepted compatibility vector, not the primary copied invite):

- Format: `petal://join/<url-encoded-access-code>`
- Native parser/handler: `apps/desktop/src-tauri/src/deep_link.rs`
- Scheme declaration: `apps/desktop/src-tauri/tauri.conf.json`

Native parsing rules:

- Scheme and `join` action are ASCII-case-insensitive.
- Decodes percent-encoded access-code text.
- Trims decoded access code and rejects empty or invalid codes.
- Tolerates one trailing slash, query, and fragment.
- Rejects wrong schemes, wrong actions, missing credentials, unencoded extra path segments, and invalid UTF-8 escapes.

Web join vectors:

- `shared/logic/joinInput.ts` parses bare access codes,
  `petal://join/<access-code>`, `https://<domain>/<name-or-label>/<access-code>`,
  `https://.../?code=<access-code>`, and `https://.../#/join/<access-code>`.
- `web-harness/src/deepLink.ts` auto-joins from access-code-bearing links;
  `web-harness/src/controls.ts` copies invite links as
  `/<name-or-label>/<access-code>` or `/<access-code>`.
- Desktop join-input: re-exported by `apps/desktop/src/lib/data/joinInput.ts`
  from the shared implementation `shared/logic/joinInput.ts` (web-harness
  imports the shared module directly).
- Desktop meeting-code generation/normalization: re-exported by
  `apps/desktop/src/lib/data/meetingCode.ts` from the shared implementation
  `shared/logic/meetingCode.ts` (web-harness imports the shared module
  directly).
- Shared invite vectors live in `contracts/petal-contracts.json` under
  `inviteLinks`; backend distribution tests and web-harness contract tests
  should pin them when invite parsing or fallback URLs change.

Files to change together:

- `web-harness/api/j.ts`
- `web-harness/vercel.json`
- `web-harness/api/_lib/slug.ts` (must stay byte-identical to
  `backend/lib/slug.ts` — duplicated, not shared, after the domain split)
- `backend/lib/slug.ts`
- `apps/desktop/src-tauri/src/deep_link.rs`
- `apps/desktop/src-tauri/tauri.conf.json`
- `apps/desktop/src/lib/data/joinInput.ts`
- `apps/desktop/src/lib/data/meetingCode.ts`
- `apps/desktop/tests/joinInput.test.ts`
- `shared/logic/joinInput.ts`
- `web-harness/src/deepLink.ts`
- `web-harness/src/controls.ts`
- `web-harness/tests/joinInput.test.ts`
- `shared/logic/meetingCode.ts` if credential normalization changes
