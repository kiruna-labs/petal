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

## #410: build macOS H.264 encoders with a level that fits the frame

### Why

The SFU answers every H.264 track with `profile-level-id=42e01f`
(Constrained Baseline, **Level 3.1**: max frame 3600 macroblocks ~= 1280x720,
MaxMBPS 108000). Petal encodes 1920x1080 cameras and Retina window shares up
to 4096 px wide. libwebrtc's ObjC `RTCVideoEncoderH264` reads the level from
the codec info it is created with and:

1. clamps the encoder frame rate to `MaxMBPS*256 / (align16(w)*align16(h))`
   -- the field logs' `Encoder frame rate setting 29 is larger than the
   maximal allowed frame rate 5` (2560x1820 share), `13` (1080p camera),
   `3`-`8` for other shares -- so VideoToolbox's `ExpectedFrameRate` is set
   to 3-13 fps while it is fed 30 fps;
2. for realtime (camera) content asks VideoToolbox for an explicit
   `kVTProfileLevel_H264_Baseline_3_1` bitstream at ~2.3x that level's max
   frame size (screen content already maps Baseline to AutoLevel).

### The fix (buildable here, no libwebrtc rebuild)

`RTCVideoEncoderH264.mm` lives inside the prebuilt libwebrtc, which is
downloaded, not built, so it cannot be patched in place. This crate DOES
compile `src/objc_video_factory.mm` itself, so the fix is a thin
`PetalLevelFittingEncoderFactory` wrapped around `RTCDefaultVideoEncoderFactory`
(the simulcast factory creates every layer's encoder through it):

- `supportedCodecs` and every optional method are forwarded untouched, so the
  SDP offer/answer is byte-for-byte what it was. The SFU codec registry (#181)
  and browser receivers see no change in negotiation.
- `createEncoder:` for H.264 rebuilds the codec info with the same profile and
  level token 2.2 (`src/h264_encoder_level.cpp`,
  `EncoderH264ProfileLevelIdFor`: `42e01f` -> `42e016`, `64001f` -> `640016`).
  Upstream maps level <= 2.2 to `kVTProfileLevel_H264_*_AutoLevel` for every
  profile family and to `GetMaxSampleRate() == 0` ("auto rate setting"), so
  VideoToolbox signals the smallest level that fits the real frame size and
  rate -- what window-share bitstreams (Baseline AutoLevel) have always carried
  to every receiver -- and the frame-rate clamp is gone.
- One `PETAL #410: H.264 encoder created with profile-level-id ...` WARNING
  line per encoder creation (the app's log sink drops libwebrtc INFO).

`h264_encoder_level.rs` exposes the pure helpers over cxx so the desktop
crate's `cargo test --lib` exercises this exact C++
(`h264_encoder_level_fits_every_published_resolution` in
`transport/publisher.rs`), including the upstream clamp formula reproducing
the logged 13/5/3/4 fps caps for Level 3.1.

Not done: advertising Level 5.x in SDP. That would change what the SFU and
browsers negotiate (#181 history: the SFU registry only matched plain
profile ids) and would need a live W2N/N2W pass before it could ship. The
negotiated `h264_profile_level_id=42e01f` in Petal's encoder-confirmation log
line is therefore unchanged by design; the encoder-side level shows in the
`PETAL #410` line, and the `maximal allowed frame rate` warnings must be
absent.

### Updating

Drop this patch if upstream derives the level from the frame size (or stops
clamping the frame rate from the negotiated level).
