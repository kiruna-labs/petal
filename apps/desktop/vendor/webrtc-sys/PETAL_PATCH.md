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

| Source | Default | Rationale |
| --- | --- | --- |
| Screensharing | driver default (set nothing) | honours WebRTC's bitrate target; text crispness comes from the link budget, not from overriding it |
| Realtime camera | CBR at the target | QUALITY starves camera frames, and the driver's untouched mode (unconstrained VBR) overshot WebRTC's target by 2.0x measured; explicit CBR on the same route cut renderer freezes from 11 to 2 |

Overrides, both strict (an unrecognized value is ignored, never silently
substituted):

- `PETAL_MF_QUALITY_MODE=1` forces QUALITY for either source (explicit
experiment). `=0` forces the non-QUALITY path, which is now the default for both
sources -- redundant, but still honoured exactly rather than ignored.
- `PETAL_MF_CAMERA_RATE_CONTROL=default|cbr|peak-vbr` applies to **realtime
camera encoders only**. It is deliberately NOT consulted for screensharing:
letting a camera selector run for screen content is what coupled the two
policies in the first place.

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
