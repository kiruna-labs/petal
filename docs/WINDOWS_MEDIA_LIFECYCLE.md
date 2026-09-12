# Windows media lifecycle inventory

This is the ownership map for one room generation. A terminal path must cancel
work before dropping its registry entry; reconnect repair is not terminal and
must keep the generation/identity guard.

| Work | Owner | Cancellation / terminal event | Safety guard |
| --- | --- | --- | --- |
| Window capture (`WindowCapture` / `SCStream`) | `session::share::ActiveShare` | `ActiveShare` stop/replacement drops capture; capture `Drop` stops native stream | share key, `started_seq`, room generation |
| Window capture frame pump | `ActiveShare::pump_abort` | stop, replacement, failed publish, or session leave aborts the task | share key + current published-track slot |
| Capture/pump watchdog | `ActiveShare::monitor` | stop/replacement aborts and awaits the monitor | share restart generation + room generation |
| Browser URL refresh | `ActiveShare::url_refresh` | `UrlRefreshTask::stop` cancels then aborts | share sequence + room generation sink check |
| Local video encoder stats | `PublishedTrack` | must be canceled on track unpublish/drop; bounded one-shot encoder probe | track instance; room connection |
| Window encoder/starvation poll | `PublishedTrack` | must be canceled on track unpublish/drop; 5-second poll | track instance; current quality/size state |
| Local screen-audio capture and pump | `ScreenAudioTrack` / audio-source registry | close queue, abort pump, unpublish, stop native capture; idempotent `stop` | audio source key, share instance, room generation |
| Remote video event loop | `start_compositor_feed` / `Subscriber` | connect-event receiver closes or generation becomes stale | `RoomGeneration` |
| macOS remote decode loop | `ReceiveWindowState` in `subscriber.rs` | per-window `CancellationToken` on unpublish, unsubscribe retirement, replacement, leave | owner identity + window id + publication SID + generation |
| Windows remote decode loop | `windows_compositor` token registry | per-window token canceled by window removal, owner removal, replacement, or room leave | owner identity + window id + publication SID + generation |
| Remote no-frame watchdog/reconciliation | compositor feed | feed exits on stale generation/closed event receiver; watchdog state is bounded | generation; authoritative room publication set |
| Remote audio stream/sink | `transport::audio` room audio task | stream ends on unsubscribe/participant departure; room task ends on generation/receiver close | identity + generation |
| Remote stats/diagnostics | diagnostics room task and per-track samples | room task ends on event receiver close or stale generation; samples are bounded | generation + track identity |
| Compositor window and frame timing | `windows_compositor` | `remove_window`, `remove_all_for`, `remove_all`; timing/token registries are cleared with removal | `WindowKey` and owner identity |
| Viewer-demand/reconcile repair | `viewer_demand` / `transport::reconcile` | generation/connection epoch invalidates stale asynchronous SDK calls | room generation + room/connection identity |

## Terminal versus recovery events

- **Terminal:** explicit stop/unpublish, confirmed `TrackUnpublished`, participant
  departure confirmed by reconciliation, room leave, or replacement of a track
  that is no longer current. Remove the track/sink state and cancel all owned
  loops.
- **Recovery:** reconnecting, transient `TrackUnsubscribed`, or an in-flight
  republish. Keep the last frame and publication authority; a replacement may
  adopt the existing window key. A stale task must not mutate the replacement.
- **Crash/missed negotiation:** retain only the bounded receiver no-frame and
  reconciliation watchdog state. It may retire a stale publication after its
  hold window, but it must not keep encoder/RTP/audio work alive indefinitely.

## Required teardown order

1. Invalidate the share/track generation and mark the resource stopped.
2. Cancel per-track tasks and close bounded queues/sinks.
3. Remove local/remote registry entries conditionally by identity/SID.
4. Perform best-effort SDK unpublish/unsubscribe without blocking visual cleanup.
5. Stop native capture/audio resources and drop remaining handles.

## Real Windows integration gate

The real Windows media gate uses `PETAL_WINDOWS_CAPTURE_HWND` to select a
real visible HWND, converts it through the same opaque capture-target registry
as the picker, and calls the production `session::start_share_token` path. The
session starts Windows.Graphics.Capture, publishes the captured frames through
LiveKit, and starts the native screen-audio adapter. The gate is intentionally
opt-in because it needs a visible target, capture/audio consent, and a running
LiveKit server; it must not be replaced by a fabricated lifecycle model.

The lower-level production test can be run on a capable Windows rig with:

```powershell
$env:PETAL_WINDOWS_MEDIA_INTEGRATION = '1'
$env:PETAL_WINDOWS_CAPTURE_HWND = '<decimal HWND>'
$env:LIVEKIT_URL = 'ws://localhost:7880'
$env:LIVEKIT_API_KEY = 'devkey'
$env:LIVEKIT_API_SECRET = 'secretsecretsecretsecretsecretsecret'
cargo test --lib session::tests::windows_share_session_runs_real_wgc_livekit_audio_lifecycle -- --exact --ignored --nocapture
```

This verifies real WGC -> `session::start_share_token` -> LiveKit observer
frames, a weak observer whose decoded width is lower after its LOW request,
an independent capable observer that remains HIGH, same-window replacement
(and a second subscription event), receiver reconnect, normal stop, a
capture-failure/missed-unpublish tail, delayed SDK unpublish, and idempotent
native audio teardown. The test also requires production process-loopback
audio to be published; the standalone system-output adapter stop is checked
again for idempotence. The deterministic receiver-quality matrix remains
separate because forcing an old-GPU/low-FPS receiver requires the
multi-machine/manual run described in the plan. The one-test TypeScript bridge
in `apps/desktop/tests/windowsWindowSharing.test.ts` executes this same Rust
gate when all prerequisites are present; otherwise it is reported as skipped
rather than pretending to be integration evidence. The always-on source contract
also checks that the executable gate retains the weak/capable observer, same-window
republish, reconnect, normal-stop, capture-failure, delayed-unpublish, and
idempotent-audio phases in that order. A skipped gate proves only compilation and
contract presence; it is not runtime evidence.

The inventory is intentionally an implementation checklist: new media tasks
must add an owner, cancellation signal, terminal event, and generation guard
here before they are merged.
