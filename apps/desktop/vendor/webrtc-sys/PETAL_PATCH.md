# Petal patches to vendored `webrtc-sys` 0.3.35

Vendored from crates.io, pinned via `[patch.crates-io]` in
`apps/desktop/src-tauri/Cargo.toml`.

## Patch 1: Windows Media Foundation H.264 codec factories

Upstream 0.3.35 only wires hardware codec factories on Linux (NVENC via
dlopen) and macOS (VideoToolbox); the Windows arm registers no hardware
factory at all, so H.264 silently falls back to OpenH264 (software). This
copy adds `src/mf/*` — MF MFT-based encoder/decoder factories compiled only
for `target_os = "windows"`. (See the Cargo.toml comment block.)

## Patch 2: ADM proxy playout/recording hardening (#787)

`src/adm_proxy.cpp` had two defects that together ALLOWED a meeting to stay
silent forever (the structural mechanism behind #787's live incident --
coherent in the code, though never reproduced live; the incident itself
remains unexplained):

1. **Init/Start split-brain.** `InitPlayout()` and `StartPlayout()` each
   evaluate `is_platform_playout_active()` independently at call time. When
   the playout enable lands between them (auto-subscribed tracks drive Init
   during connect; the app enables platform playout in its join tail), the
   platform ADM would get `StartPlayout()` without ever having been
   initialized, return `-1`, and stay silent. `StartPlayout()` (and `StartRecording()`,
   the mic-silence twin) now init-if-needed first.
2. **Silent switch failures.** `SwitchPlayoutModeIfNeeded()` ignored both
   return codes of the platform `InitPlayout()/StartPlayout()` pair, and no
   playout path logged anything. It now checks, retries once, and logs
   `LS_ERROR` on failure (these lines survive the app's per-target log
   filter, which denylists webrtc targets to `warn`).

The Rust-side halves of #787 are `session::room`'s pre-connect playout
enable and rejoin re-assert, plus `livekit`'s `reassert_playout` (see
`vendor/livekit/PETAL_PATCH.md`).

## Patch 3: source-aware Media Foundation H.264 rate control

The vendored MF encoder applied ONE rate-control policy to every caller:
`eAVEncCommonRateControlMode_Quality` (minimize QP, ignore the bitrate
target), with `PETAL_MF_QUALITY_MODE=0` as the only escape. Quality mode was
chosen for shared-window text crispness (measured QP 26 -> 16), but it is the
wrong default for screen content on a real link: it ignores the bitrate target
WebRTC's own estimate produces, so the encoder stops serving frames instead of
lowering quality.

Measured on the Windows-to-macOS route at 2560x1392, one receiver, one
simulated ladder, same binary, changing ONLY the rate-control policy:

| Policy | Receiver rendered cadence |
| --- | --- |
| QUALITY | median ~5.6 fps (766 frames / 137 s) |
| driver default (nothing set) | median ~29.5 fps (5170 frames / 317 s) |

Screen capture was healthy in both arms (WGC delivered ~30-38 frames/s,
`dropped_before_delivery=0`), and both arms ran the same configured 30 fps
ceiling, so the loss was entirely in the encoder.

### The fix

`MfH264EncoderImpl` now records the `VideoCodecMode` it was configured with and
resolves exactly one rate-control policy from it, in one place
(`ResolveRateControlPolicy` / `ApplyRateControlPolicy`):

| Source | Policy | Rationale |
| --- | --- | --- |
| Screensharing | CBR at the target | an explicit mode is vendor-independent; a driver default is whatever each GPU vendor chooses, which a cross-vendor validation cannot reason about |
| Realtime camera | CBR at the target | QUALITY starves camera frames, and the driver's untouched mode (unconstrained VBR) overshot WebRTC's target by 2.0x measured; explicit CBR on the same route cut renderer freezes from 11 to 2 |

Screen shares moved off the driver default once the real startup defect was
identified as the ALLOCATION rather than the rate-control mode: WebRTC's
estimate sat near 600 kbps for 10-35 s against a path sustaining ~14 Mbps, and
with that little per-frame budget the encoder clamped QP at its ceiling for the
whole ramp. The fix for that is `TrackPublishOptions::min_bitrate` plus the
publisher's release policy, not this mode. The QUALITY policy and the
`PETAL_MF_QUALITY_MODE` / `PETAL_MF_SCREEN_QUALITY[_VS_SPEED]` knobs are gone
outright: the driver rejected the controls QUALITY set (`quality_hr`
0x80004001, E_NOTIMPL), so the policy could never have taken effect.

One override remains, `PETAL_MF_CAMERA_RATE_CONTROL=default|cbr|peak-vbr`, and
it applies to **realtime camera encoders only** -- letting a camera selector run
for screen content is what coupled the two policies in the first place. There is
deliberately no screen-side override: the shipped screen policy is CBR.

The policy is a STATIC MFT property, so it is applied before the media types are
negotiated (an older encoder ignores a mode set afterwards). One line per
encoder creation is logged at WARNING: this application's log sink does not
capture libwebrtc INFO output, so an INFO line here would be invisible and every
rate-control A/B would silently be unreadable. `SetRates` also stops discarding
its `AVEncCommonMeanBitRate` HRESULT -- the first call and then one per 60 log
the requested target alongside the result, so a rejected target cannot
masquerade as a healthy encoder.

### Updating

Drop this patch once upstream exposes a per-encoder rate-control policy the
caller can select.

## Patch 4: a shallow screen-share MFT and a duty-cycled keyframe interval

### Why this exists

Two properties of this MFT are wrong for an interactive window share and cannot
be set from WebRTC's own `VideoEncoder` surface:

- **Its input depth is unbounded.** A stale frame is less useful than the newest
  one for a share, and the async MFT's needs-input/have-output queue is where
  staleness accumulates.
- **Its keyframe interval is whatever the driver chose (~1.15 s here).** An
  intra frame is coded with no prediction, so it costs many frame-times of drain
  time: at 1511x914 / 49 fps / 12.5 Mbps the measured intra frame was 402 kB
  against a 31.9 kB per-frame budget. A mean-bitrate CBR does not cap any single
  frame, so the interval is what decides the fraction of the time a receiver
  spends waiting for one.

### The fix

`ApplyLowLatencySettings` runs before the media types are negotiated and, for
`VideoCodecMode::kScreensharing` only, sets `MF_LOW_LATENCY`,
`CODECAPI_AVEncCommonRealTime`, zero B-pictures, one reference frame, and a
bounded pending-input window of `kMaxPendingLowLatencyInputs = 2`. The three
CODECAPI results are logged with their HRESULTs, because this MFT refuses some
of what it claims to support and a silent refusal is exactly what an A/B cannot
see.

The keyframe interval is sized from a **duty cycle** rather than left to the
driver. An initial estimate of 0.45 bytes per pixel (0.29 was measured at
qp ~20; the default carries a margin for more complex content) gives

```
intra_bytes ~= bytes_per_pixel * width * height
stall_s      = intra_bytes * 8 / target_bps
gop_frames   = stall_s / 0.05 * fps        clamped to [2 s, 10 s]
```

The interval is then refined from what the MFT actually emits: an intra frame is
identified by `MFSampleExtension_CleanPoint` where the MFT supplies it, and
thereafter by a size cut relative to the previous reference window's mean (an
absolute size cut does not survive a resolution change -- the ordinary tail
grows with pixel count at a fixed rate). Its size feeds an EMA with
`kIntraEmaAlpha = 0.5`, seeded by the first refresh, which replaces the estimate
within a few refreshes. `SetRates` re-derives the interval when the target moves
by more than 20%, so a halved target cannot leave a startup interval in place.

The 10 s cap is reported rather than silently absorbed: hitting it means no
keyframe interval can hold the duty target at this operating point, and the
answer is a resolution bound, not a longer interval.

### Updating

Drop the interval sizing if upstream exposes a keyframe schedule with a duty
parameter, and the low-latency block if `VideoEncoder::Settings` grows an input-
depth knob. Both are inert for a camera encoder.

## Patch 5: report the SPS the MF encoder actually emits

### Why this exists

The MFT's own media-type attributes are not evidence of what it encodes
(measured: `GetOutputAvailableType` reports `profile=Main`, `frame=0x0`, and a
`0xFFFFFFFF` level sentinel). A hardware MFT that accepts the configured profile
and then writes a different one into its SPS would otherwise be undetectable,
and the stream would not match what the SDP advertised.

### The fix

`h264_sps.h` carries a dependency-free Annex-B scan for `nal_unit_type 7` and
reads `profile_idc`, the constraint flags and `level_idc` straight after the NAL
header (all three precede any emulation-prevention byte, so no bit reader is
needed). `MaybeLogEmittedSps` runs that scan on keyframes only -- an SPS only
ever accompanies an IDR, so it costs at most one pass per GOP -- and logs one
line per DISTINCT triple: the first, plus any later change, which a reconfigure
to a new geometry produces because the MFT re-derives the level from the frame
size. Bounded by construction, so it cannot become a per-frame line.

The level is deliberately never set: the MFT derives it from geometry
(measured: 1080p30 -> Level 4.0, 4K30 -> Level 5.1, 4K60 -> Level 5.2), so
pinning one could only make the stream wrong.

### Updating

Safe to drop on any vendor bump; it changes no published behaviour. Fold away if
the MFT ever reports a trustworthy emitted profile through stats instead.

## #886: per-frame autorelease leak in `objc_video_frame_buffer.mm`

`native_buffer_to_platform_image_buffer` runs once per DECODED frame on
Rust/tokio decode threads, and `new_native_buffer_from_platform_image_buffer`
once per CAPTURED frame on capture threads. This file compiles under MRC, and
those Rust threads carry NO autorelease pool -- so the ObjC wrapper that
`webrtc::NativeToObjCVideoFrameBuffer` autoreleases (which retains the frame
buffer, its CVPixelBuffer, and its IOSurface) was never released. Measured
live (2026-08-25, `compositor_probe --iosurface-gate`): exactly one leaked
IOSurface per rendered frame (+29.8/s), retained until process exit --
gigabytes of graphics-ledger memory per hour per rendered window on a
receiver, invisible to RSS; the #878 field session-death mechanism. Fix: a
local `@autoreleasepool` around both function bodies. The returned +0
`CVPixelBufferRef` (decode side) stays valid past the drain because it is
owned by the caller-held frame's buffer chain, and the returned native
buffer (capture side) holds its own C++ reference. Verified: gate run of
2,678 rendered frames with `grown=0` (was +2,104 over the same duration
before the fix).

## #889: MRC leak of the ObjC video encoder/decoder factories

`objc_video_factory.mm` compiles under MRC (build.rs never passes
`-fobjc-arc`), so each `[[X alloc] init]` is a +1 the caller owns. Upstream
handed those objects to `ObjCToNativeVideoEncoderFactory` /
`ObjCToNativeVideoDecoderFactory` -- which take their OWN reference -- and
never released the locals, leaking `RTCDefaultVideoEncoderFactory`,
`RTCVideoEncoderFactorySimulcast`, and `RTCDefaultVideoDecoderFactory` on
every factory creation.

Confirmed live, not inferred: `leaks(1)` against a 2.4GB sharing session
(2026-08-25) reported 10 orphaned `webrtc::ObjCVideoEncoderFactory` roots,
one per factory creation, while `SCStream` count was 1 (so the capture
stream itself was NOT leaking). This is the encode path, which matches the
owner's observation that memory jumps when a share STARTS and stays flat
while frames merely flow.

Fix: release both locals after the native wrapper retains them, and wrap
each function in `@autoreleasepool` (these run on pool-less Rust threads --
the same hazard as this file's sibling `objc_video_frame_buffer.mm` #886
patch). Verify with `leaks <pid>` after several share/unshare cycles: the
`ObjCVideoEncoderFactory` roots must not accumulate.

## Capture-clock fallback for a frame with no Rust timestamp

### Why this exists

`VideoTrackSource::InternalSource::on_captured_frame` translated
`frame.timestamp_us()` through `webrtc::TimestampAligner` unconditionally. Rust's
`VideoFrame` timestamp defaults to zero when a capture source does not supply
one, and zero is not a usable capture-clock sample for the aligner: feeding it
repeatedly makes translated timestamps advance at roughly the aligner's 1 ms
minimum instead of the real frame cadence. The receiver renders by timestamp, so
it reproduced that spacing as reordering and stalls -- the out-of-order-frame
reports this was diagnosed from.

### The fix

Use the current WebRTC clock (`webrtc::TimeMicros()`, already read for the
aligner's `now` argument) as the aligned timestamp when
`frame.timestamp_us() <= 0`, and keep the aligner for every real timestamp. No
other behaviour changes; a source that always supplies a timestamp is unaffected.

### Updating

Drop once the vendored frame type cannot carry a zero timestamp (a non-optional
capture clock), or once `TimestampAligner` handles a zero sample itself.
