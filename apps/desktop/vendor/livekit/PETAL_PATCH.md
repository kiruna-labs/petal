# Petal patch: H.264 profile preference for native screenshares

Vendored from `livekit` 0.7.49 (crates.io), pinned via `[patch.crates-io]` in
`apps/desktop/src-tauri/Cargo.toml`.

## Why this exists

Petal window shares already publish native `is_screencast=true` video through
VideoToolbox. LiveKit's webrtc fork only enables its low-latency H.264
screenshare path (`EnableLowLatencyRateControl`, speed-over-quality, and the QP
cap) when the negotiated H.264 profile is in the High family.

The upstream Rust SDK hard-coded H.264 sender codec preferences to put
`profile-level-id=42e01f` first for browser compatibility. That is a good
default, but it prevents Mac-to-Mac Petal shares from preferring High while
retaining Constrained Baseline as a fallback.

## The fix

This patch adds a small public option:

- `H264ProfilePreference::ConstrainedBaselineFirst` (default, upstream behavior)
- `H264ProfilePreference::HighFirst`

`TrackPublishOptions` now carries that preference, and sender codec preference
selection uses it to order H.264 capabilities. High-first keeps `42e01f` after
High, so browsers that cannot decode High can still answer with the existing
Constrained Baseline profile.

Petal sets `HighFirst` only for shared-window/screen tracks. Camera tracks and
all default SDK callers keep baseline-first behavior.

## Updating

Drop this vendor patch once LiveKit exposes an upstream profile/codec-preference
API that lets callers prefer H.264 High while retaining Constrained Baseline as
fallback.

# Petal patch: live sender layer parameter read/write

## Why this exists

LiveKit's dynacast implementation already reads and writes
`RtpSender::parameters().encodings` through `LocalVideoTrack`, but the
transceiver and sender accessors were crate-private. Petal's focus policy was
therefore forced to unpublish and republish a window whenever only bitrate or
frame rate changed.

## The fix

This patch adds the public `PublishingLayerParameters` value and exposes
`LocalVideoTrack::publishing_layer_parameters` plus
`set_publishing_layer_parameters`. The setter updates only matching RID
`max_bitrate` and `max_framerate` fields, preserving the existing simulcast
layout and all other RTP parameters. Petal keeps a stable q/h/f screenshare
layout and uses these methods for Full/Reduced quality flips.

## Updating

Drop this patch once LiveKit provides an upstream public live-encoding
parameter API with equivalent per-RID bitrate/frame-rate updates. Do not
rebase it by changing the dynacast implementation: that implementation is the
behavioral precedent and must keep its existing layer-activation semantics.

# Petal patch: sender-parameter and VideoToolbox readback diagnostics

## Why this exists

Petal needs to distinguish sender limits from encoder-backend behavior while
investigating low-latency VideoToolbox settings. The pinned `libwebrtc`
bindings expose sender encoding limits and encoder implementation stats, but
do not expose VideoToolbox GOP, frame-reordering, or rate-control properties.

## The fix

Petal logs the live sender q/h/f parameter readback alongside the existing
encoder implementation/profile stats after publish starts. The log explicitly
records the unavailable backend-specific fields instead of fabricating values.

## Updating

Replace this diagnostic with direct VideoToolbox property readback when the
underlying libwebrtc binding exposes it. Until then, the sender-parameter log
is not evidence that `RealTime`, `AllowFrameReordering`, low-latency rate
control, or GOP/IDR intervals were applied.

# Documented gap: force-keyframe/PLI trigger wiring

The pinned `libwebrtc 0.3.38` and `webrtc-sys 0.3.35` surfaces were inspected
for sender/receiver keyframe, PLI, and FIR requests. They expose PLI counters
in stats but no operation that emits a request, and the vendored LiveKit API
has no lower-level escape hatch. Petal therefore does not pretend that an
`active` toggle or a sender-parameter no-op is an on-demand IDR request.

When the underlying binding gains a real request operation, add one small
public `LocalVideoTrack` method and wire it from the static-pacer resume,
`screensDidWake`, `RoomEvent::Reconnected`, and late-joiner subscription
call-sites. Validate the receiver's first-keyframe gap with the existing
native compositor stream diagnostics before removing this gap entry.

# Petal patch: remote-video transceiver accessor

## Why this exists

Petal's subscriber needs the receiver associated with a subscribed remote video
track to configure its playout delay. The SDK retains that transceiver when the
track is created, but exposed its accessor only inside the crate.

## The fix

This patch makes `RemoteVideoTrack::transceiver` public. It is purely additive
and returns the same optional cloned transceiver the SDK already stores.

## Updating

Drop this patch once the SDK exposes an equivalent public remote-video
transceiver accessor.

# Petal patch: safe remote subscription settings

## Why this exists

The upstream dimension callback sends `UpdateTrackSettings` with protobuf
defaults. `quality = 0` means Low, so a receiver's repeated geometry heartbeat
could repeatedly request Low after Petal's explicit High request. It also wrote
the receiver's requested dimensions into the remote publication's `TrackInfo`,
where callers use them as the publisher's canonical encode resolution.

## The fix

The dimension sender now suppresses an unchanged hint immediately before it
schedules its request, logs each request's actual dimensions, quality, and
disabled state, and explicitly sends `High`. The enabled-state settings path
also explicitly sends `High`; the dedicated quality path already sets its
quality. Receiver hints no longer mutate the publication `TrackInfo`.

## Updating

Drop this patch once upstream makes dimensions and quality one atomic,
non-defaulted subscription-setting operation while keeping publisher metadata
separate from receiver hints.

# Petal patch: dimension/enabled-state settings honor the last requested quality (#907)

## Why this exists

The "safe remote subscription settings" patch above made the dimension and
enabled-state `UpdateTrackSettings` senders explicitly send `High` (instead of
a protobuf-default `Low`) so a repeated geometry heartbeat could never
accidentally downgrade a subscription. That fixed one bug but created
another: #907 added a receiver-side starvation guard that deliberately
downgrades a subscription to `Low` when its requested (HIGH) simulcast layer
is starved or unreadable (QP >30). Because the dimension and enabled-state
paths hard-coded `High` unconditionally, ANY later dimension change (e.g. a
viewer resizing the remote window) or enabled-state flip (e.g. hide/show)
silently re-requested `High` and undid the downgrade -- the receiver's local
`starved` bookkeeping would say "downgraded to Low" while the SFU was
actually serving HIGH again, permanently desynced until a republish/reconnect
reset everything.

## The fix

`RemoteTrackPublication` now stores the quality last explicitly requested via
`set_video_quality` (`RemoteInner::desired_video_quality`, defaulting to
`High` so any publication that never calls `set_video_quality` keeps the
prior always-High behavior). `set_video_quality` writes it BEFORE invoking
its callback. The dimension-changed and enabled-status-changed callbacks in
`RemoteParticipant::add_publication` (`remote_participant.rs`) now read
`publication.desired_video_quality()` at send time instead of hard-coding
`proto::VideoQuality::High`, so every `UpdateTrackSettings` request this
publication sends agrees on one desired quality regardless of which trigger
(quality change, dimension change, enabled-state change) fired it.

## Updating

Drop this patch once upstream sends dimensions, enabled-state, and quality as
one atomic subscription-setting operation with an application-supplied
desired quality (superseding the "safe remote subscription settings" patch's
premise too, at that point).

## Patch: release the send-stream encoder on unpublish (Windows MF/NVENC)

Vendored from `livekit` 0.7.49; part of Petal's Windows hardware-acceleration
work (root-cause fix for the resize/republish freeze).

**Problem:** `unpublish_track` only called `rtc_engine.remove_track(sender)`
(which maps to webrtc `RemoveTrackOrError` = `RtpSenderBase::SetTrack(null)`).
The old `VideoSendStream` — and its video encoder, an async NVENC-backed MF
H.264 MFT holding a GPU encoder session — stayed alive until the PC closed.
Repeated unpublish/republish (or sequential shares) therefore accumulated GPU
sessions until MFT creation failed (GeForce 12-session cap) -> OpenH264
fallback -> 0 RTP -> receiver freeze.

**Fix:** `unpublish_track` now captures the track's transceiver and calls
`transceiver.stop()` (StopStandard) before `remove_track`, so the renegotiation
that follows drops the old m-line/SSRC and webrtc destroys the old
VideoSendStream, running `VideoEncoder::Release()` on the encoder. `stop()` is
safe because livekit creates a fresh transceiver per sender. `remove_track`
errors (engine-closed/timing race) are logged and ignored, matching
`rtc_engine`'s own TODO.

**Updating:** keep this patch while the vendored livekit lacks transceiver-stop
on unpublish; drop it once upstream tears down the send stream on unpublish.

# Petal patch 2: `PlatformAudio::reassert_playout` (#787)

`set_adm_playout_enabled` early-returns when the value is unchanged, so once
`AdmProxy::SwitchPlayoutModeIfNeeded` fails its platform `InitPlayout`/
`StartPlayout` pair, no Rust-side call could ever reach that code again — a
meeting that joined into the failure stayed silent forever, including across
the app's own rejoin repair. `reassert_playout()` toggles the enable off/on,
which re-drives the full switch (and with the webrtc-sys #787 patch, that
switch now checks, retries, and logs its return codes). Used by
`session::room`'s no-op-rejoin path so a user rejoin is a real recovery
action.

# Petal patch: reconnect-attempt logs are warn, not error

## Why this exists

`sentry-log` maps `log::error!` to a Sentry *issue*. LiveKit logs
`restarting connection... attempt: N` and `resuming connection... attempt: N`
at error on every reconnect try, plus 10s "taking too much time" monitors
that dump SDP. Those opened a Sentry backlog of lifecycle noise and drowned
real failures (`failed to handle signal`, `pc state failed`).

## The fix

Those attempt/slow-path lines are `log::warn!`. Actual failures stay
`log::error!`: `restarting/resuming connection failed`, `failed to handle
signal`, `pc state failed`.

## Updating

Keep while Petal uses `sentry-log`'s Error→event default. Drop if upstream
LiveKit logs reconnect attempts at warn/info.


## #883: Room self-cycle -- every Room connect/close leaked ~320KB forever

`room/mod.rs` registered `e2ee_manager.on_state_changed` with a closure
capturing a STRONG `Arc<RoomSession>`; `RoomSession` owns `e2ee_manager`,
which stores that closure -- a self-cycle, so no `RoomSession` (nor its
`RtcEngine`/`EngineInner`, `RtcSession`, EngineEvent/SessionEvent unbounded
channels, or either `PeerConnection`) was EVER dropped. Measured: ~320KB of
still-reachable memory per Room connect/close, linear to 1000 cycles
(+31MB/100), ~90KB more per track republish; at #841-storm rates ~1GB/hour
into the receiving client, feeding the #878 memory-pressure session deaths.
Diagnosed with temporary Drop instrumentation: `RoomSession strong=2` at
`Room` drop, and zero `RoomSession`/`SessionInner`/`EngineInner` drops
across cycles; with the fix, all three drop once per cycle and
`RoomSession strong=1` at Room drop.

Fix (two halves): the closure now captures `Arc::downgrade` and upgrades
per event (mirroring `set_session`'s existing Weak pattern three lines
above), and `E2eeManager::cleanup()` additionally clears the stored
handler. Known residual (documented in petal#883): with zero Rooms alive,
the Weak `LK_RUNTIME` singleton dies, so each later connect builds a fresh
`RtcRuntime` whose webrtc `Thread::Start` heap state is partially retained
(~123KB/room-cycle) -- an order of magnitude smaller, and per-reconnect
rather than per-republish in production.

Upstream: report both to livekit/rust-sdks (same FFI core as
livekit/python-sdks#449's report).
