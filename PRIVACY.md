# What Petal sends over the network

This documents every outbound connection Petal makes, when it happens, and how
to turn it off. It describes the software in this repository. If you use the
official hosted service, the operator of that service is Kiruna Labs, Inc.; if
you self-host, it is you.

## Always, when you use the app

### Token / rooms backend

- **When:** joining or creating a room, and listing rooms.
- **Where:** whatever `PETAL_BACKEND_URL` was set to at build time. Official
  builds point at `app.petal.live`. A source build with `PETAL_BACKEND_URL=`
  set empty uses a local debug token path instead and contacts nothing.
- **What is sent:** the room's access code or label, a generated participant id,
  and your display name.
- **Why:** the backend holds the LiveKit API secret so the client never has to.
  It returns a token scoped to one room.
- **Note:** your display name is visible to everyone else in the room, by design.

### LiveKit SFU (media and signaling)

- **When:** for the duration of a meeting.
- **Where:** the LiveKit deployment your backend points at.
- **What is sent:** your audio, your camera video if enabled, the contents of
  any window you share, telepointer coordinates, and remote-control input events
  when you have granted or been granted control.
- **Encryption:** encrypted in transit (DTLS-SRTP). **Not end-to-end encrypted**
  — the SFU decrypts in order to route streams, so the SFU operator can in
  principle access media content. See [`SECURITY.md`](SECURITY.md).
- **Retention:** Petal does not record meetings. Room *metadata* (name, access
  code) persists in LiveKit so rooms can be rejoined.

## Only in builds configured for them

These are inert unless a build supplies the corresponding value. A plain
`git clone` and build supplies none of them, so a community build does not
contact any of these.

### Auto-update

- **When:** on launch, and periodically while running.
- **Where:** the updater endpoint baked into `tauri.conf.json`. Official builds
  point at `app.petal.live/api/updater`.
- **What is sent:** the current app version and platform, plus the usual
  incidental request metadata (IP address, user agent).
- **Note:** updates are verified against a minisign public key before install.
  If you distribute a modified build, you **must** change both the feed URL and
  the signing key — see [`TRADEMARKS.md`](TRADEMARKS.md).

### Crash and diagnostic reporting

- **When:** on crash, and for selected diagnostic events.
- **Where:** the error-tracking provider (Sentry) configured at build time —
  `PETAL_SENTRY_DSN` for the desktop app, `VITE_SENTRY_DSN` for the browser
  client at `meet.petal.live`.
- **What is sent:** stack traces, app version, OS version, and Petal's own log
  breadcrumbs. Log output is passed through an allowlist-based scrubber
  (`redact_for_export` on desktop; the sensitive-string registry in the browser
  client) that masks window titles, participant identities, room names, and
  anything matching an email or URL shape before it leaves the machine.
- **Not collected:** no session replay, no screen recording, and no performance
  tracing. This is deliberate and enforced in code — Petal's UI displays other
  people's shared screens, so recording it would be a serious breach.
- **Builds without a DSN send nothing at all**, and no fallback DSN is
  substituted. Official builds of both clients configure one; if you build
  Petal yourself, omit `PETAL_SENTRY_DSN` / `VITE_SENTRY_DSN` and this
  subsystem is compiled out.

### Feedback submission

- **When:** only when you explicitly submit feedback from within the app.
  Nothing is sent unless you press submit.
- **Where:** the feedback provider (UserDispatch) configured via a public key
  at build time.
- **What is sent:** the message you typed, and — only if you tick the box — a
  diagnostic attachment. On **both** clients that attachment is a redacted
  excerpt of Petal's own log: the desktop app sends the last 256 KiB, the
  browser client the last 128 KiB, each passed through the same scrubber
  described above before it leaves your machine. The browser attachment also
  carries a small fixed header (connection state, a timestamp, and a closed set
  of UI event codes). Untick the box and no log or header is attached at all —
  only your message is sent.
- Neither client can ship a half-redacted fragment at the point the log is
  trimmed: the browser scrubs the whole text before trimming it, and the desktop
  excerpt always begins on a line boundary, so no line is ever cut through the
  middle of a value.
- **Builds without a feedback key have the feature compiled out**: no trigger
  renders and the provider's code is never loaded.

### Download endpoint

- **When:** only if you click the download link in the README or on the website.
- **Where:** `app.petal.live/api/download`. This is a normal web request; the
  app itself is not involved.

## Stored on your machine only

- Room favorites and recent rooms.
- Window layout and app preferences.
- Logs at `~/Library/Logs/Petal/petal.log` (desktop) and the in-memory session
  log (browser), which stay local unless you tick the diagnostics box on a
  feedback submission or a crash report is sent.

## Self-hosting

Running your own backend and LiveKit deployment means no data reaches Petal's
operators at all. See [`docs/SELF_HOSTING.md`](docs/SELF_HOSTING.md).
