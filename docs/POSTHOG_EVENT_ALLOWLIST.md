# PostHog event allowlist

Closed list of product events. Sentry stays the crash tool (`log::error!` →
issue). These events are how we answer “are users having a bad time?”
without opening an issue per stall sample.

Implemented in `apps/desktop/src-tauri/src/analytics.rs` (native) and
`web-harness/src/analytics.ts` (browser). Local and CI builds are keyless
and no-op; a desktop release bakes `PETAL_POSTHOG_KEY`, and a production
web-harness build bakes `VITE_PETAL_POSTHOG_KEY` (never commit the token).
Do not add events from the backend. Do not add `posthog-js`.

Dedicated project: **Petal** (id `317298`) in org Kiruna Labs, US cloud —
[project home](https://us.posthog.com/project/317298/home). Project token
lives in PostHog project settings, never in git.

Locked project settings (2026-08-17): session replay off, autocapture opt
out, JS exception autocapture off, heatmaps/surveys off, IPs anonymized,
timezone `America/Los_Angeles`, authorized URLs `petal.live` /
`app.petal.live` / `meet.petal.live`. Same PII posture as Sentry:
allowlist-first, no meeting content.

## Tooling

- **MCP (authenticated):** `posthog` in `~/.cursor/mcp.json` →
  `https://mcp.posthog.com/mcp`. Before any Petal query, `switch-project`
  to `317298` (the server defaults to whichever project last used OAuth).
- **CLI (installed, login still needed):** `posthog-cli` 0.11.3. One-time
  `posthog-cli login` (opens [CLI authorize](https://us.posthog.com/cli/authorize)),
  then `POSTHOG_CLI_PROJECT_ID=317298`. Personal API keys stay out of the
  repo.

## Properties allowed on every event

`build_version`, `os`, `os_version`, `arch`, `client` (`native` \| `web`)

Plus only the extra columns named on the event below. Duration values are
buckets (`0_10s`, `10_30s`, `30_120s`, `120s_plus`), never raw milliseconds.

`dropped_since_last` (integer, native only, added #908): present only when
greater than 0. `analytics.rs`'s `try_send` onto the bounded `QUEUE_CAP`
channel previously discarded an overflow with no counter and no trace; the
accumulated count is now embedded on the next event that actually reaches
the queue, so loss is visible in PostHog itself instead of only in a local
counter nobody looks at. The count is restored to the running total (for a
later event to carry instead) if EITHER the carrying event fails to enqueue
(`try_send` onto a still-full channel) OR it enqueues but then fails HTTP
delivery after retries are exhausted (the worker reads the value back out
of the body it just failed to send) — so it is not lost by either of those
paths, though it can lag by more than one event under sustained overflow.

It IS still lost if the process exits before a carrying event's count is
ever reported — a crash, a force-quit, or an OS-level quit (Cmd-Q, Dock →
Quit) that bypasses `quit_app`'s `analytics::flush()` (only the in-app Quit
menu item currently routes through that command; wiring the OS-level quit
paths is tracked separately, not part of #908). Do not read this property
as a complete loss-free guarantee — it closes the two most likely loss
paths (queue overflow and HTTP failure), not every path.

## Properties never allowed

Room name/slug, participant identity, window title, track sid, IP, DSN,
tokens, file paths, SDP, PCM, pixels, device names/ids/serials, display
ids, key codes/glyphs, clipboard contents, pointer coordinates, wheel
deltas.

## The events

| Event | Extra properties | Fires when |
|---|---|---|
| `meeting_joined` | — | Room connect succeeded |
| `meeting_left` | `duration_bucket`, `reconnect_count_bucket` | Leave or disconnect |
| `join_failed` | `reason`: `network` \| `no_backend` \| `token` \| `timeout` | Join did not connect |
| `share_started` | `source`: `window` \| `display` \| `picker` | A share published |
| `share_stopped` | `reason`: `user` \| `window_gone` \| `capture_failed` | A share ended |
| `remote_audio_silent` | `duration_bucket` | Remote-track watchdog `EnteredAlarm` (#787 class) |
| `remote_video_stalled` | `duration_bucket`, `source`: `stats` \| `gallery` \| `native` | Video stall watchdog, after debounce, not per flap |
| `capture_restarted` | `outcome`: `recovered` \| `failed` | In-place capture restart finished. Native only today (ScreenCaptureKit). Web has no in-place restart; the function exists for lockstep and has no production call site. |
| `reconnect` | `outcome`: `recovered` \| `failed` | LiveKit reconnect finished (not each attempt) |
| `permission_denied` | `kind`: `screen` \| `mic` \| `camera` | User denied or lost a capture/mic/camera grant. Web screen-picker dismiss (`getDisplayMedia` `NotAllowedError`) is not this event. |
| `remote_control_input` | `kind`: `click` \| `type` \| `paste` \| `scroll` | Host applied a coalesced remote-control input (see below). Web cannot inject OS input; the coalescer is tested but the host emulator must not emit. |
| `device_changed` | `kind`: `display` \| `camera` \| `mic`, `change`: `switched` \| `failed` \| `reconfigured` \| `sleep` \| `wake` | Display / webcam / mic actually changed (see below) |
| `annotation_toggled` | `state`: `on` \| `off` | #872. `draw_active` is the ONLY thing that makes the sharer overlay capture the cursor. Nothing recorded it, so a report of "I cannot click on buttons in my apps" was undiagnosable from telemetry. Emitted on a real state change only. No strokes, coordinates, or window titles. |

No new event without an explicit add to this table. Do not map
`log::error!` onto these; emit from the same call sites that today `warn!`
the watchdog, once per transition, with the buckets above.

### `remote_control_input`

Emit on the **host** when input is applied or submitted, not on the
controller send path (avoids double-count). Never pointer-`move`, never
separate `down`/`up`, never key names.

| `kind` | Coalesce | Maps from |
|---|---|---|
| `click` | Once per discrete click (`pointer` `click`, or a completed down+up). Not every mouse-down. | `petal.remote-control` `pointer` |
| `type` | Once per typing burst (idle ≥1s). A held-key repeat is still one burst. | `key` packets |
| `paste` | Once per paste. | `text` paste packets (#375) |
| `scroll` | Once per wheel burst (idle ≥500ms). Not per wheel tick. | `wheel` packets |

### `device_changed`

Once per real transition, in a meeting. Never device names. Display
reconfiguration is already bursty (`CGDisplayRegisterReconfigurationCallback`)
— coalesce like the network-change 1s debounce.

| `kind` | `change` | Call site today |
|---|---|---|
| `display` | `reconfigured` | Display hot-plug / resolution (`resilience` display monitor) |
| `display` | `sleep` / `wake` | `screensDidSleep` / `screensDidWake` |
| `camera` | `switched` | `set_camera_device` applied a different webcam |
| `camera` | `failed` | Selected / default webcam could not start |
| `mic` | `switched` | `MicDeviceChanged` (hot-plug or picker) |
| `mic` | `failed` | `MicDeviceFailed` |

Join-path hard fails (mic/speaker never came up) stay Sentry `error!` —
those are “this code path broke,” not a rate. Panic and ObjC exceptions
stay Sentry. The two structured diagnostic classes
(`capture-layout-invalid`, `camera-health`) stay the rate-limited Sentry
diagnostics they already are.
