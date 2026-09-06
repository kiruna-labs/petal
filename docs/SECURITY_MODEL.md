# Petal security model

This is the threat model behind [`SECURITY.md`](../SECURITY.md). `SECURITY.md`
tells a reporter what is in scope; this document tells a maintainer *why*, what
each component trusts, and which gaps are known and accepted. Keep them in
sync: a new trust boundary or a closed gap changes both.

Companion documents: [`docs/remote-control-trust-model.md`](remote-control-trust-model.md)
(the input-injection authorization model in full),
[`docs/CONTRACTS.md`](CONTRACTS.md) (the wire contracts every boundary below
is defined by), [`PRIVACY.md`](../PRIVACY.md), [`docs/SELF_HOSTING.md`](SELF_HOSTING.md).

## 1. Assets

In rough order of blast radius if compromised:

| Asset | Where it lives | Why it matters |
|---|---|---|
| **LiveKit API secret** | Backend env only (`LIVEKIT_API_SECRET`); never in the app or web client | Mints arbitrary room tokens for every room on the SFU: full read/write of all meetings. |
| **Updater signing key** | Release pipeline secret (`TAURI_SIGNING_PRIVATE_KEY`); only the public key ships | Signs auto-update artifacts. Compromise = code execution on every installed Mac at next update check. |
| **Apple Developer ID + notary credentials** | Release pipeline secrets | Lets an attacker ship a notarized binary under Petal's identity. |
| **Room access code** | Invite links, the participant's clipboard/chat, `petal://join/<code>` | The *only* credential for joining a meeting (see §3.1). Anyone holding it is a participant and, when remote control is on, a potential controller. |
| **Admin token** (`PETAL_ADMIN_TOKEN`) | Backend env | Kick/close any room. |
| **Shared screen content** | Sharer's GPU → H.264 → SFU → every subscriber's compositor | The product's payload. Not end-to-end encrypted; the SFU operator can read it. |
| **Remote-control input** | Data channel `petal.remote-control` → host's AX/CGEvent injection | Synthetic keyboard/mouse into a window on the sharer's machine: the most dangerous capability in the product. Native clipboard Copy/Paste also moves plain text across this authenticated boundary; the SFU can read it. |
| **Telemetry identifiers** | Sentry DSN / PostHog key baked into official builds | Low value, but leaked keys let a third party pollute the maintainers' error and analytics streams. |

## 2. Components and trust boundaries

```
 ┌────────────────────┐   HTTPS    ┌──────────────────┐   LiveKit server API   ┌─────────────┐
 │ Desktop app (Tauri)│──────────▶ │ Backend (Vercel) │ ──────────────────────▶│ LiveKit SFU │
 │ apps/desktop       │  /api/token│ backend/         │  create/list/kick      │ (managed or │
 │                    │  /api/rooms└──────────────────┘                        │ self-hosted)│
 │                    │                      ▲                                 └─────────────┘
 │                    │                      │ HTTPS /api/token                       ▲
 │                    │            ┌──────────────────┐                               │
 │                    │            │ Browser client   │   WebRTC (DTLS-SRTP) + data   │
 │                    │◀═══════════│ web-harness      │═══════════════════════════════╡
 │  (native windows,  │  media +   │ meet.petal.live  │                               │
 │   AX injection)    │  data ch.  └──────────────────┘                               │
 └────────────────────┘ ◀══════════════════════════════════════════════════════════════╯
        ▲
        │ HTTPS /api/updater (signed manifest + minisign'd archive)
 ┌──────┴─────────────┐
 │ Update feed (Blob) │
 └────────────────────┘
```

Boundaries, and what crosses each:

1. **Client → Backend** (`/api/token`, `/api/rooms/status`, `/api/admin`,
   `/api/ai-token`, `/api/gallery-token`). Caller-controlled fields are limited
   to `room`, `identity`, `displayName`, `name`, `open`; every grant field is
   fixed server-side (`docs/CONTRACTS.md` "Public LiveKit Token Grants").
   Browser callers are CORS-allowlisted (`PETAL_ALLOWED_ORIGINS`); native
   callers send no Origin. Rate limiting is per-IP and in-process.
2. **Backend → LiveKit** (server API with the API secret). The backend is the
   only holder of the secret; if it is compromised, so is every room.
3. **Client ↔ SFU ↔ Client** (WebRTC media + data channels). Encrypted in
   transit, terminated at the SFU. Data-channel topics (`petal.telepointer`,
   `petal.remote-control`, draw, AI chat) are application-level protocols with
   **no transport ACL** — any participant can publish on any topic. All
   authorization is therefore done by the *receiving* client.
4. **Remote participant → Sharer's OS** (remote-control injection). Crosses
   from "untrusted network peer" to "synthetic input into a local app". The
   host-side gate is the entire defense; see §3.3.
5. **Update feed → Installed app.** The Tauri updater verifies a minisign
   signature against the pubkey compiled into the app, and `updater.rs`
   additionally checks the archive's architecture (Mach-O slices / PE machine
   type) before staging. Builds from source ship no endpoint and never fetch.
6. **OS → App**: `petal://join/<code>` deep links (any local app or web page
   can open one), the macOS TCC grants (Screen Recording, Accessibility) the
   app holds, and the window list the app enumerates.

## 3. Attacker models

### 3.1 Unauthenticated internet attacker

Can reach `app.petal.live` and `meet.petal.live`; holds no access code.

- **Room enumeration — closed by design.** There is no public directory:
  `GET /api/rooms` is 410. `POST /api/rooms/status` answers only for
  credentials the caller presents (and, for `open:false` rooms, only with the
  access code), silently omitting everything else, so it is neither a listing
  nor an existence oracle. An outsider learns nothing about which meetings
  exist or who is in them.
- **Room creation spam / resource exhaustion.** `POST /api/rooms` is
  unauthenticated; rate limiting is per-IP, process-local, and historically
  unbounded in memory. Each Vercel instance keeps its own map, so a
  distributed caller can exceed the intended global rate.
- **Listing amplification** — closed: status is one cached `listRooms` RPC
  per instance per 3s regardless of how many callers or credentials.
- **Token minting for a known room name.** Internal room names
  (`room-<32hex>`) are unguessable; a caller must already know one. `open:
  false` rooms were historically not enforced at mint time.

Status: enumeration and amplification are closed (directory removed, status
lookup is proof-of-possession over one cached RPC); creation spam is bounded
by TTL/capped per-source and per-instance buckets; `open:false` is enforced at
mint. Until a shared rate-limit store exists, per-instance limits remain a
known limitation.

### 3.2 Malicious meeting participant

Holds a valid access code (leaked invite, ex-employee, shoulder-surfed link).

- **Is a full participant** by design: can publish/subscribe media and data.
  No per-participant authentication exists beyond possession of the code.
- **Can publish on any data-channel topic**, including
  `petal.remote-control`, because tokens grant `canPublishData` globally.
  Every receiving client must validate sender identity, room membership, and
  message shape — and does (`docs/remote-control-trust-model.md` "What IS
  enforced").
- **Can request control of a window they are not viewing.** Window IDs are
  sequential; the host verifies room membership but not subscription to that
  specific track. Known gap (#30), requires SFU subscription state the SDK
  does not expose.
- **Kick sticks.** `POST /api/admin {kick}` removes the participant from the
  SFU *and* records the identity in room metadata; the token mint refuses it
  afterwards (`backend/lib/handlers.ts`). Closed — kept here because the
  earlier gap was widely cited.
- **Can spoof telepointer/annotation identity** only within what the
  receiving client checks; identity is LiveKit-asserted, so impersonating
  another participant's identity requires the SFU's cooperation.

### 3.3 Remote controller abusing the grant

A participant who has been granted control of a shared window.

- **Intended:** keyboard/mouse into the shared window.
- **Escape from the target window** is the critical property. Injection is
  scoped by the shared window's process and, since the same-PID masking
  fix, by the specific window identity rather than PID alone (events
  could previously land on an overlapping sibling window of the same
  process). AX direct manipulation is preferred over CGEvent posting
  precisely because it targets an element, not a screen coordinate.
- **Keyboard is the sharp edge**: `CGEventPostToPid` keyboard events go to the
  process's key window, which may not be the shared window. Legacy macOS
  hosts auto-grant (trust model "Legacy macOS is auto-grant"); sharers who do
  not want this must disable remote control for the meeting.
- **Persistence:** grants are session-bound tokens; a stale or replayed
  control stream is rejected.

### 3.4 Malicious local application (on the sharer's Mac)

Runs as the same user, without admin. Out of scope per `SECURITY.md`
("requires local code execution"), listed for completeness:

- Can open `petal://join/<code>` to drop the user into an attacker's room;
  the join still requires the user to be running Petal and shows the room.
- Cannot read the access code of a meeting the user is in unless it is on the
  clipboard.
- Can enumerate the same windows Petal can; this is an OS property.

### 3.5 Compromised or hostile backend / SFU operator

- Reads all media (no E2EE) and all data-channel traffic.
- Can mint tokens for any room, kick anyone, and impersonate identities.
- **Cannot** push code to installed apps: the update path is signed with a key
  the backend does not hold, and the feed is a separate Blob store. A
  compromised backend *can* serve a stale or withheld manifest (downgrade /
  freeze), which the app mitigates only by comparing versions, not by
  requiring freshness.
- Self-hosters assume this role for their own deployment.

### 3.6 Supply chain

- Rust and npm dependencies are inventoried in `sbom/` (CycloneDX) and
  checked by `cargo deny` / `cargo audit` / `npm audit`. The vendored `screencapturekit` fork is patched locally
  (`apps/desktop/vendor/screencapturekit/PETAL_PATCH.md`; each of the five
  vendored crates under `apps/desktop/vendor/` carries one) and must be reviewed
  by hand when upstream moves.
- Release builds run in GitHub Actions with secrets injected at job time; the
  self-hosted runner holds no plaintext secrets on disk.
- Builds from source contain no telemetry keys and no update endpoint.

## 4. Security properties we commit to

1. The LiveKit secret, updater private key, and signing credentials never
   ship in any artifact or repository file.
2. Token grants are fixed server-side; no caller-supplied grant field is
   honored.
3. An update is only applied if its minisign signature verifies against the
   compiled-in key **and** its architecture matches the running binary.
4. Remote-control input only reaches the window that was shared, only while a
   live grant exists, and only from a participant the host verified is in the
   room.
5. A rendered share never goes black and never silently swaps to another
   window's content (CLAUDE.md "Never show a black frame" — a correctness
   rule that is also a confidentiality one: the compositor must never display
   a frame from a different track than the one the window is bound to).
6. Source builds make no network calls except to the LiveKit/backend the user
   configured.

## 5. Known gaps (accepted or open)

| Gap | Status | Tracking |
|---|---|---|
| No E2EE; SFU operator can read media | Accepted (documented in `SECURITY.md`) | — |
| No per-topic data-channel publish ACL | Accepted until LiveKit exposes it | trust-model doc |
| Control request not bound to viewing that track | Open, needs SFU subscription state (#30 is closed; the gap is tracked in the trust-model doc) | `docs/remote-control-trust-model.md` |
| Legacy macOS auto-grant of control | Accepted; user-controlled toggle | trust-model doc |
| Per-instance (not global) rate limiting on Vercel | Open; needs a shared store (KV seam in place) | `backend/lib/ratelimit.ts` |
| Unauthenticated room listing reveals names/occupancy | **Closed** — directory removed (410); `POST /api/rooms/status` is proof-of-possession | `backend/lib/handlers.ts` |
| Kick is not sticky | **Closed** — kicked identities are recorded in room metadata and refused at token mint | `backend/lib/handlers.ts` |
| Same-PID overlapping windows could receive injected events | **Closed** — delivery is scoped by window identity, not PID alone | `apps/desktop/src-tauri/src/remote_control.rs` |
| Updater cannot detect a frozen/withheld manifest | Accepted for now | — |

## 6. Review cadence

- Re-run `cargo audit`, `cargo deny check`, and `npm audit` in every release
  (the `sbom` workflow fails on drift; advisories are reviewed by hand).
- Any change to `backend/lib/handlers.ts`, `remote_control*.rs`, `updater.rs`,
  `deep_link.rs`, or the token grant profile must update this document and
  `docs/CONTRACTS.md` in the same PR.
- Findings from the pre-open-source review are summarized for leadership in
  `internal/docs/SECURITY_FINDINGS_2026-08-22.md`; each unresolved one is a
  GitHub issue labelled `area:security`.
