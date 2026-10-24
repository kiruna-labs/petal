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
- **Note:** while you are in a room, your display name is also shown — without
  your identity, and without them joining — to anyone who holds that room's
  invite and has the room in their list (they see it by hovering the room in
  their rooms list). A room stamped "knock to join" additionally requires its
  access code.

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
  breadcrumbs, scrubbed before they leave the machine. On desktop,
  `redact_for_export` masks room credentials, participant names and
  identities, room names, window titles, and any email address. The browser
  client masks the exact values it has registered: room names, invite codes,
  participant identities, and display names. It does not mask other email
  addresses or URLs by their shape.
- **Not collected:** no session replay, no screen recording, and no performance
  tracing. This is deliberate and enforced in code — Petal's UI displays other
  people's shared screens, so recording it would be a serious breach.
- **Turning it off:** Settings → Diagnostics → **Send crash and error reports
  to Sentry**, on macOS and Windows. It is on by default.
- **Desktop builds without a DSN send nothing at all**, and no fallback DSN is
  substituted: omit `PETAL_SENTRY_DSN` and this subsystem is inert.
- **The browser client is different.** Its build defaults to Petal's own
  Sentry DSN (`web-harness/vite.config.ts`) unless `VITE_SENTRY_DSN` overrides
  it, so a web-harness build from source reports crashes to Kiruna Labs'
  Sentry project. Set `VITE_SENTRY_DSN` to your own DSN when you self-host the
  browser client.

### Usage and performance statistics (PostHog)

- **When:** once per product event, such as joining or leaving a meeting,
  starting or stopping a share, a reconnect, or a video stall. Events are not
  sent per frame or per keystroke.
- **Where:** PostHog (US cloud), configured at build time with
  `PETAL_POSTHOG_KEY` for the desktop app and `VITE_PETAL_POSTHOG_KEY` for the
  browser client.
- **What is sent:** only the closed list of events and properties in
  [`docs/POSTHOG_EVENT_ALLOWLIST.md`](docs/POSTHOG_EVENT_ALLOWLIST.md). Every
  event carries the app version, OS, OS version, CPU architecture and
  `client` (`native` or `web`). Durations are sent as coarse buckets. Room
  names, identities, display names, window titles, device names, key codes,
  clipboard contents, pointer coordinates and tokens are never sent.
- **Anonymous id:** each install picks a random 128-bit id, stored locally
  (`analytics-id` in the app's data folder on desktop, `petal-analytics-id` in
  browser storage). It is not derived from your name, account or machine, and
  clearing it makes you a new, unrelated id.
- **No location:** every event sets `$geoip_disable` and an empty `$ip`, and
  the PostHog project anonymizes IP addresses. PostHog still sees the
  connection's IP address in transit, as any web server does.
- **Remote-control latency summary (`remote_control_latency_summary`, #370):**
  builds that include it send one event per participant per 60 seconds of
  active remote control, plus a final partial one when control ends, capped
  at 120 per control session. It is never sent per input, and builds without
  #370 do not send it at all. Each event carries integer-millisecond
  percentiles (p50, p95, max and sample count) for each stage of the
  press-to-photon path, such as the input leg, the replay, capture, encode,
  network, jitter buffer, decode and presentation. It also carries interval
  counters (input stalls, dropped frames, freezes, NACKs, PLIs, peak packet
  loss) and coarse context: role (host or controller), a random per-session
  id, the interval length, the LiveKit edge region (the server's region, not
  yours), whether both sides use the same edge (`same_edge`), whether the SFU
  is self-hosted (`self_hosted_sfu`), the ICE candidate type and protocol, the
  window codec and whether a content change was detected (`content_change`).
  It never carries identities, room names, window titles, coordinates or key
  data. The point is to find which stage makes remote control feel slow,
  which is why it uses milliseconds instead of the coarse buckets above. The
  exact field list is in the allowlist.
- **Turning it off:** Settings → Diagnostics → **Share anonymous usage and
  performance statistics** on desktop (macOS and Windows), or the same switch
  in the profile card in the browser (click your avatar on the home screen).
  It is on by default. While it is off, nothing is sent. On desktop, events
  from the first moments after launch are held until the app has read your
  saved choice, so an opt-out also covers startup.
- **Builds without a key send nothing at all.** A build from source without
  `PETAL_POSTHOG_KEY` / `VITE_PETAL_POSTHOG_KEY` never contacts PostHog.

### Feedback submission

- **When:** only when you explicitly submit feedback from within the app.
  Nothing is sent unless you press submit.
- **Where:** the feedback provider (UserDispatch) configured via a public key
  at build time.
- **What is sent:** the message you typed, your email address, and — only if
  you tick the box — a diagnostic attachment. On **both** clients that
  attachment is a redacted excerpt of Petal's own log: the desktop app sends
  the last 256 KiB, the browser client the last 128 KiB, each passed through
  the same scrubber described above before it leaves your machine. The browser
  attachment also carries a small fixed header (connection state, a timestamp,
  and a closed set of UI event codes). Untick the box and no log or header is
  attached at all — only your message and email address are sent.
- **Your email address** is required on every feedback form so we can reply
  to you. It is only checked to look like an address; it is never verified,
  and nothing is sent to it to confirm it. It travels only in UserDispatch's
  own email field — never inside the diagnostic attachment, and Petal does not
  write it to its logs, crash reports, or analytics.
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
- While a browser tab is in a meeting, that meeting's access code in the
  tab's session storage, so reloading the tab rejoins. It stays in that tab,
  is never sent anywhere, and is cleared when you leave the meeting; the
  browser drops it with the tab's session. After a disconnect you did not
  ask for, it is kept while the disconnect notice is up (a reload then
  rejoins), and cleared when you go back to the home screen.
- Window layout and app preferences, including your crash-report and
  usage-statistics choices (`petal-usage-stats` in the browser).
- The random anonymous id described under usage statistics.
- The email address you last sent feedback with, so the form can fill it in
  next time (you can change it there). The desktop app clears it on a factory
  reset; in the browser it goes when you clear the site's data.
- Logs at `~/Library/Logs/Petal/petal.log` (desktop) and the in-memory session
  log (browser), which stay local unless you tick the diagnostics box on a
  feedback submission or a crash report is sent.

## Self-hosting

Running your own backend and LiveKit deployment means no data reaches Petal's
operators at all. See [`docs/SELF_HOSTING.md`](docs/SELF_HOSTING.md).
