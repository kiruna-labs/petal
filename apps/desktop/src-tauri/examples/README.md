# Rust Example Probes

These examples are diagnostic probes for Petal's native media, LiveKit, audio, and compositor paths. Run them from `apps/desktop/src-tauri`.

Most LiveKit probes need:

```sh
LIVEKIT_URL=ws://localhost:7880
LIVEKIT_API_KEY=devkey
LIVEKIT_API_SECRET=secret
```

Some examples load `apps/desktop/.env` with `dotenvy`; `audio_probe` and `mic_capture_probe` require those variables directly in the process environment.

## Probes

### `capture_probe`

```sh
cargo run --example capture_probe -- [window_id] [--geometry]
```

Verifies ScreenCaptureKit capture for a real on-screen window without LiveKit. With no `window_id`, it lists shareable windows and picks the first one. Requires macOS Screen Recording permission for the example binary or terminal.

`--geometry` (or `PETAL_PROBE_GEOMETRY=1`) runs the **capture-geometry integrity harness** (#531). It captures the same window three ways — direct window id, system-picker filter, and a deliberately orientation-swapped configuration — decodes the delivered NV12 Y plane, and asserts numerically that the raster's aspect/orientation matches the source window's real point geometry and that non-black content fills the raster. The swapped pass manufactures #531's exact failure (ScreenCaptureKit letterboxes landscape content into the top-left of a portrait buffer) and then applies the capture-layout gate's reconfiguration request, mirroring `session/share.rs`, to prove the stream converges instead of publishing a padded raster. `PETAL_PROBE_DUMP_DIR=<dir>` also writes each pass's Y plane as a PGM.

This is a throwaway experiment loop for capture-side geometry work — no LiveKit, no second peer, no bundle — not cockpit apparatus.

### `share_lifecycle_probe`

```sh
LIVEKIT_URL=ws://localhost:7889 LIVEKIT_API_KEY=devkey LIVEKIT_API_SECRET=... \
  cargo run --example share_lifecycle_probe -- [--seconds 35] \
    [--placeholder-demand] [--no-sid-guard]
```

Two-peer, single-process **share-lifecycle harness** (#355) that measures both halves of "a fresh share appears too small then disappears within ~6 seconds" against a real SFU, instead of eyeballing either:

- **geometry** — publishes a known 1920x1080 simulcast source and records the pixel dimensions the subscriber actually decodes, so "too small" becomes `received != source` with a percentage.
- **timeline** — samples window presence every 100ms and timestamps every room event with its track sid, so "disappears" becomes an exact `t=` plus the state transition that caused it.

At `t=6s` it reproduces `session/share.rs`'s viewer-demand downsize-hold republish (publish the new track first, then unpublish the old — both named `petal-window-<id>`), the sequence that made the pre-fix receiver tear down a live window. This is also the only way to observe the ordering the sid guard depends on: that a real SFU delivers `TrackSubscribed(new)` **before** `TrackUnpublished(old)`. No unit test on the pure guard function can establish that.

Two positive controls prove the harness can see each failure before its absence is trusted:

- `--placeholder-demand` advertises compositor's pre-first-frame 640x400 placeholder, as `viewer_demand` did before #355's fix — the SFU then pins the subscriber to a half layer (960x540, 50% of source) for the whole run.
- `--no-sid-guard` replaces `should_remove_window` with the pre-fix unconditional teardown — the window is removed ~0.3s after the republish.

A local dev SFU is enough — no second Mac, no bundle, no Screen Recording grant. Experiment loop for share-lifecycle work, not cockpit apparatus and not a runtime diagnostic subsystem (internal/docs/COURSE_CORRECTION.md §2.1).

#### `--late-joiner` mode (#357)

```sh
LIVEKIT_URL=ws://localhost:7889 LIVEKIT_API_KEY=devkey LIVEKIT_API_SECRET=... \
  cargo run --example share_lifecycle_probe -- --late-joiner \
    [--trials 20] [--legacy-subscribe]
```

Same two-peer, single-process, real-SFU setup, asking #357's question instead: does a peer that connects *after* a share is already running actually receive it, and how fast? Each trial runs an **early** observer (connects before anything is published) alongside the **late** joiner, both through the real `RoomConnection::connect` + `take_compositor_events()` pair that `session::join_room` hands to `start_compositor_feed`. The early observer is the in-run positive control: it cannot be affected by #357, so if it sees nothing the run is thrown out rather than read as a pass.

`--legacy-subscribe` is the second control: the late joiner discards the connect-time receiver and calls `room.subscribe()` a signaling round trip later, exactly as `RoomConnection::connect` did before #364. It must demonstrably miss shares, or a default-mode pass proves nothing.

Note the control trips on "demonstrably misses", not "never succeeds". #357's `TrackSubscribed` half is a race the pre-fix code lost by a round trip, and on a loopback SFU that round trip is occasionally short enough for the old path to still win (measured: 19/20 misses, not 20/20). The `Connected`-snapshot half has no race in it — a receiver registered after connect can never see it — and is reported separately.

### `publish_probe`

```sh
cargo run --example publish_probe -- [window_id] [room_name]
```

Captures a real window and publishes it to a LiveKit room as an H.264 video track. Loads `LIVEKIT_URL`, `LIVEKIT_API_KEY`, and `LIVEKIT_API_SECRET` from `apps/desktop/.env`.

For a parity-controlled real-capture diagnostic, pass
`--expected-capture-width 1600 --expected-capture-height 900`. The example
checks frame #1 before connecting/publishing and exits with
`INVALID_CAPTURE_RASTER` on any physical-pixel mismatch. The #613 runner also
requires the matching `CAPTURE_RASTER_VERIFIED 1600x900` marker before it
accepts publisher startup.

For a capture-apparatus health check that never loads LiveKit credentials or
connects to a room, use the exact target window ID with
`--capture-preflight-only`:

```sh
cargo run --example publish_probe -- 12345 --source real \
  --expected-capture-width 960 --expected-capture-height 600 \
  --capture-preflight-only
```

It emits one `CAPTURE_PREFLIGHT_RESULT` JSON record containing only frame
dimensions and counters for accepted frames, no-image-buffer samples,
layout/pixel-format rejections, and asynchronous ScreenCaptureKit errors. Only
an accepted exact-raster frame emits `CAPTURE_PREFLIGHT_READY`; no pixels are
persisted. This is an operator-authorized ScreenCaptureKit preflight, not a
LiveKit or latency measurement.

### `subscribe_probe`

```sh
cargo run --example subscribe_probe -- [room_name] \
  [--seconds 30] [--steady-after-ms 8000] [--first-frame-timeout-ms 15000] \
  [--measurement-window-file /path/to/start-end-epoch-us.txt]
```

Joins the same room as `publish_probe` through the production
`transport::Subscriber` path, counts frames/gaps, and reports latency from
embedded sender timestamps plus inbound RTC jitter/decode stage deltas.
`PETAL_PROBE_DUMP=/path.csv` preserves every sample. Loads LiveKit env from
`apps/desktop/.env`.

`publish_probe --source synthetic` replaces only ScreenCaptureKit input with a
fixed-cadence NV12 source. It still publishes through
`RoomConnection::connect_and_publish` and `PublishedTrack::push_frame`, so
metadata stamping, codec, and simulcast behavior remain identical to the real
capture probe. When both probes receive the same `--measurement-window-file`,
publisher cadence/overwrite counters and receiver observations use the exact
same absolute 16–28 second boundary.
The synthetic source remains exactly 1600×900; the expected real-capture flags
do not change it.

For #613's bounded, pre-registered receiver start-order experiment, build both
examples once and run:

```sh
cargo build --locked --example publish_probe --example subscribe_probe
../scripts/run-issue613-receiver-start-order.sh --matrix synthetic
../scripts/run-issue613-receiver-start-order.sh --matrix real
# Or run both serially:
../scripts/run-issue613-receiver-start-order.sh --matrix both
```

The runner aligns every arm and both sender/receiver counter snapshots to
publisher age 16–28 seconds, irrespective of receiver start order. Receiver
samples are selected by decoded-callback `receive_us` wall-clock; embedded
`capture_us` describes source/capture age and supplies the capture→decode lower
bound. It separately records publisher pushed cadence/capture-slot overwrites,
end-to-end publisher frame-id gaps, and RTC receiver drops. The reported
capture-callback-to-decoded-callback latency is a lower bound, not glass-to-
glass latency. Synthetic isolation and real-capture confirmation use the same
rotated pairs and positive-control gates. A single matrix always produces
`NO_PRODUCT_CONCLUSION`; confirmation requires both valid matrices to agree.
The runner owns and tears down its local SFU,
deterministic nonactivating target window, publisher, and subscriber processes.
A `FALSIFIED` verdict is a valid result and must not be turned into a product
patch.

### `share_latency_probe`

```sh
cargo run --example share_latency_probe -- [window_id] [room_name] \
  [--seconds 30] [--steady-after-ms 8000] [--pin-lowest | --inject-delay-ms N]
PETAL_SHARE_LADDER=legacy-bottom30 cargo run --example share_latency_probe -- 12345 petal-613-latency
```

Single-process, real-capture latency probe (#613/#299): captures a real
window, publishes it through the product's own `publish_window_at` path, joins
a subscriber peer in the same process, and reports capture-stamp →
decoded-frame latency from the frame-metadata `user_timestamp` — one wall
clock, no cross-process offset correction. This is a **lower bound on
glass-to-glass** (it excludes compositor enqueue and display presentation; the
presentation-inclusive matrix is `run-issue613-presentation-latency.mjs`
above). `startup_layer_probe` takes the same measurement from a synthetic
source; the `publish_probe` + `subscribe_probe` pair takes it across two
processes under the receiver-start-order runner. This probe is the lightweight
per-ladder experiment loop between those: set `PETAL_SHARE_LADDER` and read
the `LADDER :` banner line, which reports the ladder **as computed** at the
real capture size, never the value you exported. `legacy-bottom30` is the
measurement-only ladder that differs from `legacy` in exactly the bottom
rung's framerate cap, isolating bottom-rung cadence from bottom-rung size —
the run that established cadence is not the latency lever (2026-07-28, n=6:
p50 175.2ms vs legacy's 138.0ms, encoder utilisation 74%→~90%).

Two positive controls prove the instrument can see what it claims to measure:

- `--pin-lowest` requests a fixed 160x90 (below every rung of every ladder, so
  it can only resolve to the live ladder's bottom rung, and the constant is
  not derived from the ladder — the check is not circular). A FAIL means the
  build is stale or the ladder is not applied, and the run is void.
- `--inject-delay-ms N` withholds each captured frame for N ms after its
  capture stamp via a per-frame delay line (never a pump-loop sleep, which
  collapses cadence and moves the number for the wrong reason). Reported
  latency must rise by ~N or the instrument is blind.

The report separates the ramp from the steady window, buckets latency by the
live ladder's decoded rung, prints RTCStats per-stage deltas over the same
window (encode, pacer, assembly, jitter buffer, decode), and warns when frames
arrive without a `user_timestamp`. `PETAL_PROBE_DUMP=/path.csv` preserves
every observation — recompute p50/p95 from the CSV and they must equal the
printed percentiles. Capture side mirrors `session/share.rs`'s latest-wins
frame slot (a queue here once billed ~122ms of probe-side backlog to the
pipeline). Needs Screen Recording access and a local SFU or `.env` LiveKit
credentials. Experiment loop for #613/#299 work, not cockpit apparatus.

### `startup_layer_probe`

```sh
cargo run --example startup_layer_probe -- [--seconds 12] \
  [--pin-lowest | --no-demand | --quality-then-dimensions]
```

Two-peer, single-process **startup-layer harness** (#299) answering "why does a
fresh share start blurry and slow?" as one measurement rather than two. A Full
window share defaults to `q` (3W/4 x 3H/4, capped 30fps) and `h` (source, up to
60fps). The probe resolves every selected ladder at runtime, so the decoded
buffer's own dimensions name the layer exactly.

It publishes through the real `publisher::full_share_publish_options` and drives
the real `viewer_demand::startup_demand_decision`, replaying the receiver
lifecycle the compositor actually produces (Open while the panel is a hidden
placeholder, a geometry/DPI settle at 150ms still hidden, the reveal, the 2s
heartbeat). It reports the initial layer, time to the first source-resolution
frame, decoded fps over the decoded-frame interval within the first 10s, and
every layer transition.

`--pin-lowest` is the positive control: it requests the selected ladder's bottom
rung for the whole run and must reach that rung, or the harness cannot see the
failure and no clean reading from another mode is trustworthy.

`--no-demand` sends no track settings at all — what the browser peer does before
its tile exists, since `adaptiveStream` is off in web-harness and its only
`setVideoQuality`/`setVideoDimensions` call is tile-backed and post-subscribe.
This is what established that the SFU ramps every fresh subscription from the
lowest layer regardless of demand.

`--quality-then-dimensions` probes #590 part 2: the vendored SDK builds the
dimensions `UpdateTrackSettings` with `..Default::default()`, so it carries an
implicit `quality: LOW`. Whether that actually undoes a prior HIGH request is
measured here, not assumed.

Needs a local SFU only — no second Mac, no bundle, no Screen Recording grant.
Read "Two traps that silently invalidate a local two-peer media run" in
`docs/TESTING.md` before trusting a `<no frames decoded>` result or any timing
from it. Experiment loop for #299 work, not cockpit apparatus.

### `compositor_probe`

```sh
cargo run --example compositor_probe -- [room_name]
```

Subscribes to a real LiveKit window-share track and paints decoded H.264 frames into the same native display-layer type used by the app compositor, but inside a plain AppKit window. Use with `publish_probe` in another process.

For #613's presentation-inclusive matrix it additionally accepts
`--window-x`, `--window-y`, `--window-width`, `--window-height`, and
`--nonactivating` for a pre-registered non-key destination, plus
`--enqueue-delay-ms 200` for the positive control. It emits
`DESTINATION_CROP_PX` and `display_enqueued=` evidence; the delay is applied
immediately before the native display-layer enqueue, not in the decoder.

### `presentation-latency-observer.swift` (#613)

`../scripts/run-issue613-presentation-latency.mjs` is the owned-process
coordinator for the deferred native→web and web→native, same-display
presentation matrix. It runs 120 post-warmup, unique Gray-code pairs in each
idle and one-core-50%-CPU cell, records only local timing/counter CSV and log
evidence, and requires p95 below 100 ms in every measured cell. The +200 ms
control must increase paired p50 by 150–250 ms before the baseline is
accepted: native→web delays the captured/stamped frame in this example with
`publish_probe --presentation-delay-ms 200` while retaining the real visible
remote video; web→native delays only compositor enqueue. One ScreenCaptureKit
observer receives concrete source/destination window IDs and an explicit
coordinator-selected display descriptor, validates the CG/SCWindow-to-crop
physical transform, and decodes both calibrated crops in memory. It persists no pixels
and rejects ambiguity, overlap/out-of-bounds, incomplete frames, post-ready
decode loss, counter regressions, unpaired destination generations, or cadence
outside 25–35 paired generations per second.

The observer always uses the full-display ScreenCaptureKit filter; it never
falls back to a direct-window capture. If ScreenCaptureKit reports zero display
candidates, it emits `INVALID_OBSERVER_DISPLAY_UNAVAILABLE`, writes no valid
CSV/result cell, and the coordinator records a zero-cell invalid artifact. This
is apparatus evidence, not a product-latency failure. Resume only after
`SCShareableContent` reports one matching display candidate.

Build the two native examples before an operator-authorized live run:

```sh
cargo build --locked --example publish_probe --example compositor_probe
../scripts/run-issue613-presentation-latency.mjs --direction both --load both
```

The coordinator writes an `owned-process-lease.tsv` beside the evidence and
uses exact detached process groups for SFU, Vite, Chrome, source, publisher,
compositor, observer, and CPU worker cleanup. The worker emits measured
`process.cpuUsage`/wall utilization and the loaded cell is invalid outside
45–55% of one logical core. `--self-test` is static only;
it does not open a browser, SFU, or window.

### `hold_last_frame_probe`

```sh
cargo run --example hold_last_frame_probe                  # fixed: frame must be HELD
cargo run --example hold_last_frame_probe -- --stale-guard  # control: pixels must LEAVE the screen
```

The native half of CLAUDE.md's "never show a black frame" rule (#627), proved by
sampled composited screen pixels across a forced frame gap rather than by events —
"held the last frame" and "went blank quietly" emit the same events. Needs no
LiveKit: it drives the real `DisplayLayer` in a plain AppKit window and acts on the
real `teardown_decision`.

Prefer the wrapper, which runs both directions and refuses to report a pass unless
the control tripped first:

```sh
scripts/verify-no-black-frame-native.sh
```

Needs a window server and Screen Recording access; it exits `3` as HARNESS INVALID
rather than passing or failing when capture is denied. `scripts/ci-local.sh`
compile-checks it only. See docs/TESTING.md.

### `audio_probe`

```sh
cargo run --example audio_probe -- publish [room_name]
cargo run --example audio_probe -- subscribe [room_name]
```

Publishes a synthetic 440 Hz audio tone in one process and subscribes in another. Verifies real Opus/LiveKit audio transport by checking frame arrival, non-silence, and dominant frequency. Requires direct `LIVEKIT_*` env vars.

### `mic_capture_probe`

```sh
cargo run --example mic_capture_probe -- [room_name]
```

Initializes the real platform audio device module, enumerates recording/playout devices, publishes a real microphone track, checks outbound RTP stats, and smoke-tests mute/unmute. Requires direct `LIVEKIT_*` env vars and usable audio hardware/permissions.

### `mint_token`

```sh
cargo run --example mint_token -- --room petal-room-example --identity web-tester --publish true --subscribe true
```

Mints a LiveKit JWT for manual use in `web-harness/`. Loads LiveKit env from `apps/desktop/.env`. Pass the actual LiveKit room name if you want to join the same room as a native Petal client.

### `bare_window_probe`

```sh
cargo run --example bare_window_probe
```

Opens a minimal AppKit window with no LiveKit, Tokio, or Tauri. Use it to isolate WindowServer/AppKit environment problems from networking or Petal app logic.

### `frontmost_probe`

```sh
cargo run --example frontmost_probe -- --watch-pid <pid> --seconds 12 --interval-ms 5
cargo run --example frontmost_probe -- --watch Petal --seconds 12
```

Samples `NSWorkspace.frontmostApplication` at a fixed high rate (~5-10ms) and
prints a timeline of foreground-ownership transitions, then a summary giving
the number of episodes, total, and longest contiguous foreground time for the
watched app. Turns "did the app flash to the front?" into a number in
milliseconds instead of an eyeball judgement -- built for #120 (share-start
foreground flash), and reusable for any activation/focus regression.

It observes any app by pid or name and never touches product code, so the same
before/after measurement works against a `tauri dev` binary, a packaged
`Petal.app`, or a standalone AppKit probe. Prefer `--watch-pid`: substring
matching on a name/bundle id false-positives easily (watching `desktop` also
matches `com.anthropic.claudefordesktop`) and silently produces a wrong
verdict.


### `camera_cadence_probe`

```sh
cargo run --release --example camera_cadence_probe -- subscribe petal-549 40
cargo run --release --example camera_cadence_probe -- publish   petal-549 30
```

Measures the published-webcam pipeline's per-stage cadence using a
**synthetic** NV12 source, so the same numbers can be taken from an aarch64
process and from an x86_64 process under Rosetta and compared directly (#549,
`internal/docs/COURSE_CORRECTION.md` §3.2 -- there is no second Mac).

It drives the real product path (`RoomConnection::publish_camera` +
`PublishedTrack::push_nv12`) and never touches AVFoundation, so it needs no
camera hardware and no camera TCC grant, and the input bytes are identical
across architectures: any difference between two runs is the pipeline, not the
webcam. Frames are a pre-rendered ring of moving hard-edge bars, so the
measured loop contains no pattern-generator cost.

Publisher output: `push_fps`, `convert_ms` (NV12->I420), `capture_frame_ms`
(handoff into libwebrtc), `dropped_push`, and `late_ticks` (frames that missed
the 30fps budget). Subscriber output: decoded fps, inter-frame gap
p50/p95/p99/max, stalls over 100ms, and sender-side frame-id gaps.

Build the x86_64 slice with `MACOSX_DEPLOYMENT_TARGET=13.0` -- see
`docs/TESTING.md`, "Rosetta x86_64 peer tier". Always measure in `--release`;
debug timings are dominated by unoptimized plane copies and are not comparable.

Note the subscriber here is the *native* `Subscriber`, which is the transport
view of what the sender emits. The product's webcam receiver is the webview
gallery bridge (`src/lib/data/galleryBridge.ts`); this probe deliberately does
not model the receiver's layer-selection policy.
