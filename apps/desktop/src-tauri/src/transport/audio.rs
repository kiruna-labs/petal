//! Audio: mic capture + publish, remote audio subscribe/playback, and mute
//! (SPEC.md §4.9). Companion to `publisher.rs`/`subscriber.rs` -- same
//! module boundary (every `livekit::*` audio call lives here), same "thin
//! module, not a trait yet" scoping note as `transport/mod.rs`'s own doc
//! comment.
//!
//! ## Mic-capture API chosen: `livekit::PlatformAudio`, not `cpal`, not
//! hand-rolled `AVCaptureDevice`/CoreAudio via objc2
//!
//! Checked all three options against the actual `livekit` 0.7.49 source
//! (`Cargo.lock` already pins `webrtc-sys 0.3.35`/`libwebrtc 0.3.38`
//! transitively; neither `cpal` nor `coreaudio-rs` appears anywhere in
//! `Cargo.lock` -- grepped directly, not assumed) before deciding:
//!
//! - **`cpal`**: would mean capturing raw PCM ourselves, then pushing frames
//!   into a `NativeAudioSource` by hand (the same shape as this module's
//!   video path: `capture.rs` -> `push_frame`). Fully viable, but it means
//!   hand-building a second independent audio device pipeline (device
//!   enumeration, hot-swap, resampling to whatever rate/channel-count the
//!   `NativeAudioSource` wants) AND separately wiring echo cancellation --
//!   `cpal` captures raw audio with zero APM; without WebRTC's own AEC/NS/AGC
//!   in the loop, a laptop's speaker output would round-trip right back into
//!   its own mic. That's a real, non-trivial subsystem to build correctly.
//! - **Hand-rolled `AVCaptureDevice`/CoreAudio via objc2**: the house style
//!   for other native APIs in this codebase (`capture.rs`'s ScreenCaptureKit,
//!   `menubar.rs`'s AppKit), but for the *same* reason as `cpal` above: raw
//!   capture with no APM baked in, and CoreAudio's own AEC (`kAUVoiceIOProperty`
//!   等) would need to be wired up by hand, duplicating what libwebrtc already
//!   ships and tunes.
//! - **`livekit::PlatformAudio`** (chosen): the `livekit` crate we already
//!   depend on for video re-exports libwebrtc's own **Audio Device Module**
//!   (ADM) -- WebRTC's cross-platform mic-capture + speaker-playout layer,
//!   CoreAudio-backed on macOS (confirmed directly in
//!   `platform_audio/mod.rs`'s own doc comments: "macOS: Uses CoreAudio for
//!   device management"). `PlatformAudio::new()` acquires it,
//!   `PlatformAudio::rtc_source()` returns `RtcAudioSource::Device`, and
//!   `LocalAudioTrack::create_audio_track(name, source)` publishes straight
//!   from the mic -- no manual frame pump, no manual resampling, and
//!   critically: WebRTC's own Audio Processing Module (echo cancellation +
//!   noise suppression + AGC) is wired in automatically
//!   (`AudioProcessingOptions::default()` -> `echo_cancellation: true,
//!   noise_suppression: true, auto_gain_control: true`, applied inside
//!   `PlatformAudio::new()` itself -- read directly in
//!   `platform_audio/mod.rs`/`processing.rs`, not assumed). This is a
//!   dependency we already link and already fought the linker for (see
//!   `transport/mod.rs`'s M0 blocker writeup) -- adding `cpal` or hand-rolled
//!   CoreAudio FFI would be a SECOND independent audio stack fighting for the
//!   same hardware, with none of the APM benefit. Zero new crates, zero new
//!   linker surface: the clear right call per the task's own "pick the path
//!   that's actually cleanest given what's already a dependency" framing.
//!
//! `PlatformAudio` is reference-counted (`Arc`-backed internally) across
//! however many `PlatformAudio::new()` calls happen -- the ADM is acquired
//! once and shared, released only when the last handle drops. This module
//! keeps exactly one `PlatformAudio` instance alive for the process's mic
//! publish AND (separately) uses a second instance for the subscribe side's
//! playout enablement -- see `subscribe_room_audio`'s doc comment for why
//! playout needs its own `PlatformAudio::new()` call even though this
//! process is a publisher too.
//!
//! ## Opus / in-band FEC / DTX / RED -- explicit vs. default, verified
//!
//! SPEC.md §4.9: "Opus, with in-band FEC and DTX. Add RED... on lossy
//! paths." Checked against the real crate source rather than assumed
//! (same rigor the M0 video path needed for `VideoEncoderBackend`):
//!
//! - **Codec = Opus**: there is no `audio_codec` field on
//!   `TrackPublishOptions` at all (unlike `video_codec` -- checked
//!   `room/options.rs` directly). LiveKit's WebRTC audio path only offers
//!   Opus; it isn't a choice to make, so there's nothing to set explicitly.
//! - **DTX**: `TrackPublishOptions::dtx` defaults to `true`
//!   (`room/options.rs`'s `Default for TrackPublishOptions`) -- this module
//!   does not need to override it, but sets it explicitly anyway (see
//!   `audio_publish_options()` below) so the SPEC requirement is visible at
//!   the call site rather than relying on a silent default that could
//!   change upstream.
//! - **RED**: `TrackPublishOptions::red` defaults to `true`, but this module
//!   explicitly overrides it to `false` (see `audio_publish_options()` below).
//!   RED (RFC 2198 redundant Opus encoding) is a real interop hazard with
//!   browser/mobile subscribers: several WebRTC audio decode stacks (older
//!   WebKit/Safari, some Chrome-for-Android builds) accept a RED-wrapped Opus
//!   payload at the signaling/SFU-forwarding layer with zero error, then
//!   silently fail to decode it -- producing complete, undetectable silence
//!   rather than a dropped connection or a JS-visible error. This reproduces
//!   as one-way audio: desktop-to-desktop is unaffected (native decoder
//!   handles RED fine), but web/mobile participants hear nothing from a
//!   desktop peer while still being heard themselves (their own publish
//!   never used RED). In-band FEC (see below) is unconditional in this SDK
//!   and covers most of RED's packet-loss-resilience benefit, so disabling
//!   RED trades a marginal loss-recovery improvement for correctness across
//!   every subscriber platform. Re-enable only if RED is negotiated
//!   per-subscriber-capability, not blanket-on.
//! - **In-band FEC**: NOT a `TrackPublishOptions` field at all -- checked
//!   `rtc_engine/peer_transport.rs` directly, which hard-codes
//!   `a=fmtp:111 minptime=10;useinbandfec=1` into every Opus SDP media
//!   description the SDK generates, unconditionally. There is no knob to
//!   flip; in-band FEC is always on for every Opus track this SDK publishes.
//!   Confirmed via the crate's own SDP-munging unit test
//!   (`peer_transport.rs`'s test asserting the exact fmtp line appears).
//!
//! So: DTX and RED are defaults confirmed via source + set explicitly for
//! documentation; in-band FEC is unconditional and unconfigurable (in a good
//! way -- it's always on). None of this required overriding anything, unlike
//! the M0 video path's `VideoEncoderBackend`/`video_encoding` overrides,
//! which WERE required because the camera-oriented defaults were wrong for
//! this app's screenshare use case.
//!
//! ## Echo cancellation / APM
//!
//! SPEC.md §4.9: "echo cancellation via the OS/WebRTC APM." Confirmed
//! (not assumed) via `platform_audio/processing.rs`: WebRTC's own software
//! Audio Processing Module is ALWAYS used on desktop (macOS has no hardware
//! AEC path the way iOS's VPIO does -- `AudioProcessingOptions`'s own doc
//! comment: "Desktop: Hardware processing is not available. WebRTC's
//! software Audio Processing Module (APM) is always used."), and
//! `PlatformAudio::new()` calls `configure_audio_processing(
//! AudioProcessingOptions::default())` itself, which has
//! `echo_cancellation: true, noise_suppression: true, auto_gain_control:
//! true`. This is automatic, not something this module has to opt into --
//! documented here so it's clear it was verified, not assumed.
//!
//! ## Mute semantics: `LocalAudioTrack::mute()`/`unmute()`, not
//! unpublish/republish
//!
//! Unlike the video path's quality-switch problem (`publisher.rs`'s doc
//! comment: no public API to mutate a *live* video track's encoding
//! parameters, so quality switches go through unpublish+republish),
//! `LocalAudioTrack` has a genuine public `mute()`/`unmute()` pair (checked
//! directly in `local_audio_track.rs`) that calls the SDK's own internal
//! `set_muted` plumbing -- this sends a `MuteTrackRequest` to the server and
//! flips `RtcAudioTrack::set_enabled`, WITHOUT unpublishing/renegotiating
//! anything. This is exactly LiveKit's intended mute mechanism (mirrors
//! every official LiveKit client SDK's mic-mute button), so `set_mic_muted`
//! below just calls it -- no unpublish/republish dance needed for audio.
//!
//! ## Per-user identity
//!
//! Same explicit stand-in as everywhere else in this phase's scope
//! (`session::DEV_IDENTITY`, `telepointer::DEV_USER_ID`): this process
//! publishes its mic track on whatever room connection `session.rs` already
//! made under `DEV_IDENTITY`. No new identity concept introduced for audio.

use crate::sync_ext::MutexExt;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use livekit::options::TrackPublishOptions;
use livekit::prelude::*;
use livekit::webrtc::audio_frame::AudioFrame;
use livekit::webrtc::audio_source::native::NativeAudioSource;
use livekit::webrtc::audio_source::AudioSourceOptions;
use livekit::webrtc::audio_source::RtcAudioSource;
use livekit::webrtc::audio_stream::native::NativeAudioStream;
use livekit::{AudioProcessingOptions, PlatformAudio};
use serde::Serialize;

use crate::session::RoomGeneration;

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("failed to acquire platform audio device (no mic/speaker hardware available?): {0}")]
    PlatformAudio(#[from] livekit::AudioError),
    #[error("failed to publish microphone track: {0}")]
    Publish(#[from] livekit::RoomError),
}

fn default_recording_device(
    devices: &[livekit::RecordingDeviceInfo],
) -> Option<&livekit::RecordingDeviceInfo> {
    #[cfg(target_os = "windows")]
    match crate::windows_audio_device::default_recording_device_id() {
        Ok(id) => {
            if let Some(device) = devices.iter().find(|device| device.id.as_str() == id) {
                return Some(device);
            }
            log::warn!("audio: Windows default recording endpoint was not in ADM enumeration");
        }
        Err(error) => log::warn!("audio: {error}; falling back to first recording device"),
    }
    devices.first()
}

fn default_playout_device(
    devices: &[livekit::PlayoutDeviceInfo],
) -> Option<&livekit::PlayoutDeviceInfo> {
    #[cfg(target_os = "windows")]
    match crate::windows_audio_device::default_playout_device_id() {
        Ok(id) => {
            if let Some(device) = devices.iter().find(|device| device.id.as_str() == id) {
                return Some(device);
            }
            log::warn!("audio: Windows default playout endpoint was not in ADM enumeration");
        }
        Err(error) => log::warn!("audio: {error}; falling back to first playout device"),
    }
    devices.first()
}

/// The mic's LiveKit track name (`docs/CONTRACTS.md`'s "Microphone track",
/// pinned by `contracts/petal-contracts.json`'s `micTrack` on both sides). A
/// `pub(crate)` constant so #713's reconnect publication-repair health check
/// (`session::share::reconnect_publication_health`) checks the SAME literal
/// `prepare_microphone` publishes under, instead of a second hardcoded copy
/// that could drift.
pub(crate) const MIC_TRACK_NAME: &str = "petal-mic";

/// SPEC.md §4.9's audio publish policy, made explicit rather than relying on
/// (matching, but silent) `TrackPublishOptions::default()` values -- see
/// module doc comment for what's verified default vs. what's unconfigurable.
/// `red: false` is an explicit override of the SDK default -- see module doc
/// comment's "RED" section for the web/mobile silent-audio interop hazard
/// this avoids.
fn audio_publish_options() -> TrackPublishOptions {
    TrackPublishOptions {
        source: TrackSource::Microphone,
        dtx: true,
        red: false,
        ..Default::default()
    }
}

fn audio_publish_summary(options: &TrackPublishOptions) -> String {
    fn on_off(enabled: bool) -> &'static str {
        if enabled {
            "on"
        } else {
            "off"
        }
    }

    format!(
        "Opus, DTX {}, RED {}, in-band FEC always-on",
        on_off(options.dtx),
        on_off(options.red)
    )
}

/// This process's published microphone track: the live `PlatformAudio`
/// handle (keeps the ADM's recording side alive), the resulting
/// `LocalAudioTrack` (for `mute`/`unmute`), and the muted-state cache so
/// `is_muted()` doesn't need to round-trip into the SDK for a value the
/// caller almost always already knows it just set.
pub struct MicTrack {
    /// Keeps the platform ADM's mic-capture side alive for as long as this
    /// `MicTrack` exists -- dropping the last `PlatformAudio` handle
    /// releases the ADM (see module doc comment on ref-counting). Also used
    /// by `refresh_default_recording_device` (SPEC.md §4.8 device hot-swap)
    /// to re-enumerate/switch the recording device in place.
    audio: PlatformAudio,
    track: LocalAudioTrack,
    muted: AtomicBool,
    /// The recording device id this track was (re-)pinned to, as of the last
    /// successful enumeration -- read+compared by
    /// `refresh_default_recording_device` to detect "the default device
    /// changed" without needing any push notification from the SDK (see
    /// module doc comment on device hot-swap: `PlatformAudio` has no
    /// device-change callback, only a pull `recording_devices()` iterator).
    /// `None` if we were never able to determine a current device (e.g. zero
    /// recording devices enumerated).
    current_device: Mutex<Option<DeviceSnapshot>>,
    /// issue #28: `true` once the user explicitly picked a recording
    /// device (Settings mic select -> `set_recording_device`, or a persisted
    /// preference applied at publish time). Precedence vs. `resilience.rs`'s
    /// auto-hot-swap poll (`refresh_default_recording_device`, called every
    /// 2s): while pinned, the poll does NOT chase the system default device
    /// around -- the user's explicit choice wins. The one exception: if the
    /// pinned device disappears from enumeration entirely (unplugged), the
    /// poll falls back to the first available device and CLEARS the pin, so
    /// audio keeps flowing rather than staying wedged on a dead device.
    user_pinned: AtomicBool,
}

/// A cheap snapshot of the recording device selected by the ADM. `id` is the
/// stable per-device GUID (`RecordingDeviceId`, per its own doc comment,
/// "persists across device hot-plug events on desktop").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceSnapshot {
    id: livekit::RecordingDeviceId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordingDeviceRefresh {
    Unchanged,
    Switched(String),
    Failed(String),
}

impl MicTrack {
    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::SeqCst)
    }

    /// This mic's current LiveKit track SID. Used by #713's reconnect
    /// publication-repair health check to tell whether the vendored SDK's
    /// own unpublish+republish-on-restart (`handle_restarted`) actually left
    /// a live `petal-mic` publication behind, the same way `PublishedTrack::
    /// sid()` backs the window-share repair check in `session::share`.
    pub(crate) fn track_sid(&self) -> TrackSid {
        self.track.sid()
    }

    /// Republish this mic's already-created `LocalAudioTrack` onto `room`'s
    /// local participant (#713 reconnect repair). Confirmed in the vendored
    /// SDK (`vendor/livekit/src/room/mod.rs`'s `handle_restarted`) that a
    /// full reconnect's own republish attempt UNPUBLISHES before it tries to
    /// republish -- so when that attempt times out, this participant is left
    /// with no `petal-mic` publication at all, not a stale one; a plain
    /// `publish_track` call is therefore correct here, no `unpublish` first.
    /// Reuses the SAME `LocalAudioTrack`/`AudioSource` this `MicTrack` was
    /// built with, so capture itself never stopped -- only the SFU-side
    /// publication was lost.
    pub(crate) async fn republish_after_reconnect(&self, room: &Arc<Room>) -> Result<(), AudioError> {
        room.local_participant()
            .publish_track(LocalTrack::Audio(self.track.clone()), audio_publish_options())
            .await?;
        log::info!("audio: mic track republished after reconnect publication repair");
        Ok(())
    }

    /// Record the user's latest mute intent synchronously so UI reads stay
    /// immediate even when the SDK call is dispatched onto Tauri's runtime.
    pub fn cache_muted_state(&self, muted: bool) {
        self.muted.store(muted, Ordering::SeqCst);
    }

    /// Mute or unmute the published mic track in place -- see module doc
    /// comment for why this is a real `LocalAudioTrack::mute`/`unmute` call
    /// and not an unpublish/republish cycle.
    pub fn set_muted(&self, muted: bool) {
        // The session applies the persisted (muted) mic state right after
        // join; a test rig that asked to publish unmuted must not be silently
        // re-muted a millisecond later -- see `publish_unmuted_for_tests`.
        if muted && publish_unmuted_for_tests() {
            log::warn!("audio: PETAL_AUDIO_PUBLISH_UNMUTED=1 -- ignoring mute request");
            return;
        }
        self.cache_muted_state(muted);
        if muted {
            self.track.mute();
        } else {
            self.track.unmute();
        }
        log::info!(
            "audio: mic track {}",
            if muted { "muted" } else { "unmuted" }
        );
    }

    /// SPEC.md §4.8 device hot-swap: "observe... default-device changes.
    /// Mic unplugged... re-negotiate the affected track in place, keep the
    /// call up."
    ///
    /// `PlatformAudio` (checked directly, see module doc comment) has no
    /// push callback for "the default recording device changed" -- only a
    /// pull `recording_devices()` iterator and an explicit
    /// `switch_recording_device(id)` hot-swap call. So this is a poll: call
    /// this periodically (see `resilience.rs`'s device-watch loop) and it
    /// compares the current first-enumerated device against
    /// `self.current_device`. If they differ (a new device became first --
    /// which is how CoreAudio's own default-device reordering surfaces
    /// through this iterator, confirmed by this crate's own
    /// `RecordingDeviceInfo::index` doc comment: "may change when devices
    /// are added/removed"), calls `switch_recording_device` to hot-swap the
    /// live track onto the new default, keeping the same `LocalAudioTrack`/
    /// published-track identity (no unpublish/republish, unlike the video
    /// quality-switch problem) -- the call stays up throughout.
    ///
    /// Returns `Some(new_device_name)` if a switch actually happened (for
    /// logging/toast purposes), `None` if nothing changed or the switch
    /// failed (failures are logged, not propagated -- a failed hot-swap
    /// attempt should not crash the audio pipeline; the old device stays in
    /// use and the next poll tries again).
    /// issue #28: explicitly pin the mic to a user-chosen recording
    /// device by its stable GUID (`RecordingDeviceId`), hot-swapping the live
    /// track in place via the same `switch_recording_device` mechanism the
    /// auto-hot-swap poll uses. Sets `user_pinned` so the resilience poll
    /// stops chasing the system default (see `user_pinned`'s doc comment for
    /// the precedence rules). Returns the switched-to device's human name.
    pub fn set_recording_device(&self, device_id: &str) -> Result<String, String> {
        let devices: Vec<_> = self.audio.recording_devices().collect();
        let Some(target) = devices.iter().find(|d| d.id.as_str() == device_id) else {
            return Err(format!("recording device not found: {device_id}"));
        };
        self.audio
            .switch_recording_device(&target.id)
            .map_err(|e| format!("failed to switch recording device: {e}"))?;
        self.user_pinned.store(true, Ordering::SeqCst);
        let mut guard = self.current_device.lock_unpoisoned();
        *guard = Some(DeviceSnapshot {
            id: target.id.clone(),
        });
        log::info!(
            "audio: mic hot-swapped to '{}' (user-selected)",
            target.name
        );
        Ok(target.name.clone())
    }

    pub fn use_default_recording_device(&self) -> Result<String, String> {
        let devices: Vec<_> = self.audio.recording_devices().collect();
        let Some(default) = default_recording_device(&devices) else {
            return Err("no recording devices available".to_string());
        };
        self.audio
            .switch_recording_device(&default.id)
            .map_err(|e| format!("failed to switch to default recording device: {e}"))?;
        self.user_pinned.store(false, Ordering::SeqCst);
        *self.current_device.lock_unpoisoned() = Some(DeviceSnapshot {
            id: default.id.clone(),
        });
        log::info!(
            "audio: mic hot-swapped to system default '{}'",
            default.name
        );
        Ok(default.name.clone())
    }

    pub fn refresh_default_recording_device(&self) -> RecordingDeviceRefresh {
        let devices: Vec<_> = self.audio.recording_devices().collect();

        // User-pinned precedence (issue #28, see `user_pinned` doc
        // comment): while the user has explicitly chosen a device, the
        // default-chasing logic below is suspended. Only if the pinned device
        // vanished from enumeration (unplugged) do we fall back to the first
        // available device -- and clear the pin, since the pinned device no
        // longer exists to honor.
        if self.user_pinned.load(Ordering::SeqCst) {
            let mut guard = self.current_device.lock_unpoisoned();
            let pinned_present = match guard.as_ref() {
                Some(snapshot) => devices.iter().any(|d| d.id == snapshot.id),
                None => false,
            };
            if pinned_present {
                return RecordingDeviceRefresh::Unchanged;
            }
            let Some(first) = default_recording_device(&devices) else {
                log::warn!("audio: user-selected recording device disappeared and no fallback recording devices are available");
                return RecordingDeviceRefresh::Failed(
                    "Microphone disconnected — check input device".to_string(),
                );
            };
            return match self.audio.switch_recording_device(&first.id) {
                Ok(()) => {
                    self.user_pinned.store(false, Ordering::SeqCst);
                    *guard = Some(DeviceSnapshot {
                        id: first.id.clone(),
                    });
                    log::warn!(
                        "audio: user-selected recording device disappeared -> fell back to '{}'",
                        first.name
                    );
                    RecordingDeviceRefresh::Switched(first.name.clone())
                }
                Err(e) => {
                    log::warn!(
                        "audio: pinned recording device disappeared but fallback switch failed: {e}"
                    );
                    RecordingDeviceRefresh::Failed(
                        "Microphone disconnected — check input device".to_string(),
                    )
                }
            };
        }

        let Some(first) = default_recording_device(&devices) else {
            let had_previous = self.current_device.lock_unpoisoned().is_some();
            if had_previous {
                log::warn!(
                    "audio: no recording devices available after microphone was previously active"
                );
                return RecordingDeviceRefresh::Failed(
                    "Microphone disconnected — check input device".to_string(),
                );
            }
            return RecordingDeviceRefresh::Unchanged;
        };
        let snapshot = DeviceSnapshot {
            id: first.id.clone(),
        };

        let mut guard = self.current_device.lock_unpoisoned();
        if guard
            .as_ref()
            .is_some_and(|current| current.id == snapshot.id)
        {
            return RecordingDeviceRefresh::Unchanged;
        }
        let previous = guard.replace(snapshot.clone());

        // First poll after construction: just record the baseline, don't
        // treat "no prior snapshot" as a hot-swap event (there was nothing
        // to switch away from) -- `publish_microphone` already seeds this
        // baseline, so `previous` should normally be `Some` by the first
        // real poll, but this guards the case where enumeration returned
        // nothing at construction time (e.g. a device arriving after
        // startup with zero devices initially present).
        if previous.is_none() {
            return RecordingDeviceRefresh::Unchanged;
        }

        match self.audio.switch_recording_device(&first.id) {
            Ok(()) => {
                log::info!(
                    "audio: default recording device changed -> switched to '{}'",
                    first.name
                );
                RecordingDeviceRefresh::Switched(first.name.clone())
            }
            Err(e) => {
                log::warn!(
                    "audio: default recording device changed but switch_recording_device failed: {e}"
                );
                // Roll back the recorded snapshot so the next poll retries
                // the switch instead of assuming it already succeeded.
                *guard = previous;
                RecordingDeviceRefresh::Failed(
                    "Microphone disconnected — check input device".to_string(),
                )
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlayoutDeviceSnapshot {
    id: livekit::PlayoutDeviceId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlayoutDeviceRefresh {
    Unchanged,
    Switched(String),
    Failed(String),
}

/// Retains the shared ADM's speaker side and owns its default-vs-explicit
/// selection state for one joined media session on either platform.
pub struct SpeakerPlayout {
    audio: PlatformAudio,
    current_device: Mutex<Option<PlayoutDeviceSnapshot>>,
    user_pinned: AtomicBool,
}

impl SpeakerPlayout {
    fn new(preferred_playout_device: Option<String>) -> Result<Self, AudioError> {
        let audio = PlatformAudio::new()?;
        let devices: Vec<_> = audio.playout_devices().collect();
        log::info!(
            "audio: playout enabled ({} playout device(s) available)",
            devices.len()
        );

        let mut current_device =
            default_playout_device(&devices).map(|device| PlayoutDeviceSnapshot {
                id: device.id.clone(),
            });
        let mut user_pinned = false;
        if let Some(wanted_id) = preferred_playout_device {
            match devices.iter().find(|device| device.id.as_str() == wanted_id) {
                Some(device) => match audio.switch_playout_device(&device.id) {
                    Ok(()) => {
                        current_device = Some(PlayoutDeviceSnapshot {
                            id: device.id.clone(),
                        });
                        user_pinned = true;
                        log::info!("audio: applied preferred playout device '{}'", device.name);
                    }
                    Err(error) => log::warn!(
                        "audio: failed to apply preferred playout device '{}': {error} -- using default",
                        device.name
                    ),
                },
                None => log::warn!(
                    "audio: preferred playout device {wanted_id} not present -- using default"
                ),
            }
        }

        Ok(Self {
            audio,
            current_device: Mutex::new(current_device),
            user_pinned: AtomicBool::new(user_pinned),
        })
    }

    pub fn set_playout_device(&self, device_id: &str) -> Result<String, String> {
        let devices: Vec<_> = self.audio.playout_devices().collect();
        let Some(target) = devices
            .iter()
            .find(|device| device.id.as_str() == device_id)
        else {
            return Err(format!("playout device not found: {device_id}"));
        };
        self.audio
            .switch_playout_device(&target.id)
            .map_err(|error| format!("failed to switch playout device: {error}"))?;
        self.user_pinned.store(true, Ordering::SeqCst);
        *self.current_device.lock_unpoisoned() = Some(PlayoutDeviceSnapshot {
            id: target.id.clone(),
        });
        log::info!(
            "audio: playout hot-swapped to '{}' (user-selected)",
            target.name
        );
        Ok(target.name.clone())
    }

    pub fn use_default_playout_device(&self) -> Result<String, String> {
        let devices: Vec<_> = self.audio.playout_devices().collect();
        let Some(default) = default_playout_device(&devices) else {
            return Err("no playout devices available".to_string());
        };
        self.audio
            .switch_playout_device(&default.id)
            .map_err(|error| format!("failed to switch to default playout device: {error}"))?;
        self.user_pinned.store(false, Ordering::SeqCst);
        *self.current_device.lock_unpoisoned() = Some(PlayoutDeviceSnapshot {
            id: default.id.clone(),
        });
        log::info!(
            "audio: playout hot-swapped to system default '{}'",
            default.name
        );
        Ok(default.name.clone())
    }

    pub fn refresh_default_playout_device(&self) -> PlayoutDeviceRefresh {
        let devices: Vec<_> = self.audio.playout_devices().collect();
        if self.user_pinned.load(Ordering::SeqCst) {
            let mut current = self.current_device.lock_unpoisoned();
            let pinned_present = current
                .as_ref()
                .map(|snapshot| devices.iter().any(|device| device.id == snapshot.id))
                .unwrap_or(false);
            if pinned_present {
                return PlayoutDeviceRefresh::Unchanged;
            }
            let Some(default) = default_playout_device(&devices) else {
                return PlayoutDeviceRefresh::Failed(
                    "Speaker disconnected — check output device".to_string(),
                );
            };
            return match self.audio.switch_playout_device(&default.id) {
                Ok(()) => {
                    self.user_pinned.store(false, Ordering::SeqCst);
                    *current = Some(PlayoutDeviceSnapshot {
                        id: default.id.clone(),
                    });
                    log::warn!(
                        "audio: user-selected playout device disappeared -> fell back to '{}'",
                        default.name
                    );
                    PlayoutDeviceRefresh::Switched(default.name.clone())
                }
                Err(error) => {
                    log::warn!(
                        "audio: pinned playout device disappeared but fallback switch failed: {error}"
                    );
                    PlayoutDeviceRefresh::Failed(
                        "Speaker disconnected — check output device".to_string(),
                    )
                }
            };
        }

        let Some(default) = default_playout_device(&devices) else {
            if self.current_device.lock_unpoisoned().is_some() {
                return PlayoutDeviceRefresh::Failed(
                    "Speaker disconnected — check output device".to_string(),
                );
            }
            return PlayoutDeviceRefresh::Unchanged;
        };
        let snapshot = PlayoutDeviceSnapshot {
            id: default.id.clone(),
        };
        let mut current = self.current_device.lock_unpoisoned();
        if current
            .as_ref()
            .is_some_and(|selected| selected.id == snapshot.id)
        {
            return PlayoutDeviceRefresh::Unchanged;
        }
        let previous = current.replace(snapshot);
        if previous.is_none() {
            return PlayoutDeviceRefresh::Unchanged;
        }

        match self.audio.switch_playout_device(&default.id) {
            Ok(()) => {
                log::info!(
                    "audio: default playout device changed -> switched to '{}'",
                    default.name
                );
                PlayoutDeviceRefresh::Switched(default.name.clone())
            }
            Err(error) => {
                log::warn!("audio: default playout device changed but switch failed: {error}");
                *current = previous;
                PlayoutDeviceRefresh::Failed(
                    "Speaker disconnected — check output device".to_string(),
                )
            }
        }
    }

    /// Re-drive the ADM's playout mode for a rejoin re-assert (#787). Delegates
    /// to the held `PlatformAudio` so the session never needs the raw handle.
    pub fn reassert_playout(&self) {
        self.audio.reassert_playout();
    }
}

/// Enable this process's speaker playout so subscribed remote participants'
/// audio tracks are actually audible, not just received.
///
/// ## Why this needs its own `PlatformAudio` handle even though
/// `publish_microphone` already created one
///
/// `PlatformAudio` is reference-counted and shares one underlying ADM (see
/// module doc comment) -- calling `PlatformAudio::new()` again here is
/// cheap and reuses the same handle if one is already alive. This is kept
/// as a SEPARATE call (rather than threading `MicTrack`'s internal handle
/// through) so playout has its own independent lifecycle from the mic
/// publish: a room connection with `can_subscribe` but no local mic
/// published yet (e.g. publish races, or a future "listen only" mode)
/// should still hear other participants. Returns the `SpeakerPlayout`
/// wrapper so the caller keeps ADM playout alive for the room's lifetime
/// (dropping it would tear down playout) AND retains a handle able to
/// `refresh_default_playout_device()`. Unwrapping this to the bare
/// `PlatformAudio` is exactly the #867 defect: it silently discards the only
/// thing that can follow a default-device change mid-call.
///
/// No manual "attach remote track to speaker" step exists or is needed:
/// once the platform ADM's playout side is enabled
/// (`PlatformAudio::new()`'s own `set_adm_playout_enabled(true)` call,
/// confirmed directly in `platform_audio/mod.rs`), WebRTC's native audio
/// pipeline automatically mixes and renders every subscribed remote audio
/// track through the selected playout device -- confirmed against the
/// crate's own `tests/platform_audio_test.rs` two-participant test, which
/// only asserts `RoomEvent::TrackSubscribed` fires and never calls any
/// separate "start playback" API.
pub fn enable_managed_playout(
    preferred_playout_device: Option<String>,
) -> Result<SpeakerPlayout, AudioError> {
    SpeakerPlayout::new(preferred_playout_device)
}

/// Synchronously prepared microphone resources. This is deliberately detached
/// from room/session state so `join_room` can run preparation on the blocking
/// pool and safely abandon its join handle at the terminal deadline (#569).
pub struct PreparedMicrophone {
    audio: PlatformAudio,
    track: LocalAudioTrack,
    muted: bool,
    current_device: Option<DeviceSnapshot>,
    user_pinned: bool,
    publish_summary: String,
}

impl PreparedMicrophone {
    pub fn mute_for_cleanup(&self) {
        self.track.mute();
    }

    pub fn track_sid(&self) -> TrackSid {
        self.track.sid()
    }

    pub fn into_mic_track(self) -> MicTrack {
        MicTrack {
            audio: self.audio,
            track: self.track,
            muted: AtomicBool::new(self.muted),
            current_device: Mutex::new(self.current_device),
            user_pinned: AtomicBool::new(self.user_pinned),
        }
    }
}

/// `PETAL_DISABLE_AUDIO` -- the single predicate both session paths (macOS
/// `session::room`, Windows `session_stub`) use, so a video-only run means
/// the same thing on both.
///
/// `0`/`false`/`no`/`off` mean audio stays ENABLED. This previously treated
/// any non-empty value as "disable", so `PETAL_DISABLE_AUDIO=0` -- the exact
/// incantation docs/TESTING.md, docs/TEST_PLAN.md and the cockpit launcher's
/// own error message all prescribe for audio runs -- silently skipped the mic
/// publish and speaker playout. AUD-N2W caught it: the web listener waited
/// 15s for a track that a run configured FOR audio never published.
pub(crate) fn audio_is_disabled(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        let value = value.trim();
        !value.is_empty()
            && !matches!(
                value.to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            )
    })
}

/// #812 (journey AUD-04): substitute a deterministic 440Hz tone for the
/// machine's microphone INPUT, while leaving every other part of the publish
/// path untouched -- same track name, same `TrackPublishOptions`, same
/// `publish_track` call, same republish-repair wiring. Only the samples'
/// origin changes.
///
/// This exists because "can a web listener hear the Mac?" has no automated
/// answer otherwise: a CI/agent machine's real mic records a silent room, so
/// a green run would prove nothing (and #787 is the standing lesson about
/// audio checks that pass while nobody can hear anything). It deliberately
/// does NOT test CoreAudio capture itself -- no automated test on this
/// machine can -- and the scenario that uses it says so.
///
/// Off unless `PETAL_AUDIO_SYNTH_TONE=1`; never reachable in a normal run.
pub(crate) fn synthetic_tone_capture_enabled() -> bool {
    std::env::var("PETAL_AUDIO_SYNTH_TONE").as_deref() == Ok("1")
}

/// `PETAL_AUDIO_PUBLISH_UNMUTED=1` -- publish the mic unmuted and refuse the
/// session's join-time mute. **Only honored together with
/// `PETAL_AUDIO_SYNTH_TONE=1`**, i.e. only when the mic input is a synthetic
/// tone and no real microphone is involved.
///
/// Petal joins MUTED by default (correct product behaviour), so an automated
/// audio run that only publishes measures digital silence at the receiver and
/// cannot tell that apart from a broken pipeline. That ambiguity cost most of
/// a day on #821: every local measurement was of a correctly-muted track.
/// Rigs with no UI to click unmute with set this instead.
///
/// The synth-tone coupling is not tidiness, it is the safety property. Without
/// it, this variable leaking into a dev shell would join a REAL microphone hot
/// and turn the mute button into a silent no-op -- and worse, `SessionState::
/// set_mic_muted` stores the desired state synchronously for UI reads before
/// the async apply this refuses, so the UI would show MUTED while the mic
/// transmitted. That is the worst failure shape a comms product has.
pub(crate) fn publish_unmuted_for_tests() -> bool {
    synthetic_tone_capture_enabled()
        && std::env::var("PETAL_AUDIO_PUBLISH_UNMUTED").as_deref() == Ok("1")
}

const SYNTH_TONE_HZ: f32 = 440.0;
const SYNTH_SAMPLE_RATE: u32 = 48_000;
const SYNTH_CHANNELS: u32 = 1;
const SYNTH_FRAME_MS: u32 = 10;

/// Build the tone source and spawn the pump that feeds it. The pump lives as
/// long as the process: a mic track can be muted/unmuted and republished
/// across reconnects, and a source that stopped producing would look exactly
/// like the silence this scenario exists to detect.
fn synthetic_tone_source() -> RtcAudioSource {
    let samples_per_frame = SYNTH_SAMPLE_RATE / (1000 / SYNTH_FRAME_MS);
    // APM off, for the same reason ai_chat::voice gives for the assistant's
    // voice: this is already clean synthesized audio, and noise suppression
    // reads a steady pure tone as noise and gates it to digital silence.
    // Measured: with `AudioSourceOptions::default()` the web listener decoded
    // 4.02s of samples with totalAudioEnergy delta EXACTLY 0 while the pump
    // logged 4,500 captured frames -- silence manufactured between the source
    // and the encoder, invisible to every counter on either side.
    let source = NativeAudioSource::new(
        AudioSourceOptions {
            echo_cancellation: false,
            noise_suppression: false,
            auto_gain_control: false,
        },
        SYNTH_SAMPLE_RATE,
        SYNTH_CHANNELS,
        1000,
    );
    let pump = source.clone();
    tauri::async_runtime::spawn(async move {
        let mut phase: f32 = 0.0;
        let mut frames: u64 = 0;
        let phase_step = 2.0 * std::f32::consts::PI * SYNTH_TONE_HZ / SYNTH_SAMPLE_RATE as f32;
        let mut ticker =
            tokio::time::interval(Duration::from_millis(u64::from(SYNTH_FRAME_MS)));
        loop {
            ticker.tick().await;
            let mut frame =
                AudioFrame::new(SYNTH_SAMPLE_RATE, SYNTH_CHANNELS, samples_per_frame);
            {
                let data = frame.data.to_mut();
                for sample in data.iter_mut() {
                    *sample = (phase.sin() * (i16::MAX as f32) * 0.5) as i16;
                    phase += phase_step;
                    if phase > 2.0 * std::f32::consts::PI {
                        phase -= 2.0 * std::f32::consts::PI;
                    }
                }
            }
            if let Err(error) = pump.capture_frame(&frame).await {
                log::warn!("audio: synthetic tone capture_frame failed: {error}");
                return;
            }
            frames += 1;
            if frames % 3000 == 0 {
                log::info!("audio: synthetic tone pump alive ({frames} frames captured)");
            }
        }
    });
    log::warn!(
        "audio: PETAL_AUDIO_SYNTH_TONE=1 -- publishing a synthetic {SYNTH_TONE_HZ}Hz tone \
         INSTEAD of microphone input (test hook; the rest of the publish path is unchanged)"
    );
    RtcAudioSource::Native(source)
}

/// Acquire/configure the platform microphone and create its local track.
/// Contains no room/session access and performs no await.
pub fn prepare_microphone(
    preferred_recording_device: Option<String>,
    initial_muted: bool,
) -> Result<PreparedMicrophone, AudioError> {
    let audio = PlatformAudio::new()?;

    let recording_devices: Vec<_> = audio.recording_devices().collect();
    log::info!(
        "audio: {} recording device(s), {} playout device(s) available",
        recording_devices.len(),
        audio.playout_devices().count()
    );
    for device in &recording_devices {
        log::info!(
            "audio: recording device [{}] '{}'",
            device.index,
            device.name
        );
    }

    // issue #28: apply the user's persisted recording-device preference
    // (if any) BEFORE creating the track, so the mic publishes from the
    // chosen device on join rather than switching after the fact.
    // `switch_recording_device` handles the not-yet-recording case (it only
    // does the stop/restart dance when recording was already initialized --
    // confirmed in `platform_audio/mod.rs` source). A stale preference
    // (device unplugged since it was saved) falls back to the default with a
    // log line, never fails the publish.
    let preferred = preferred_recording_device;
    let mut pinned = false;
    if let Some(ref wanted_id) = preferred {
        match recording_devices
            .iter()
            .find(|d| d.id.as_str() == wanted_id)
        {
            Some(device) => match audio.switch_recording_device(&device.id) {
                Ok(()) => {
                    pinned = true;
                    log::info!(
                        "audio: applied preferred recording device '{}'",
                        device.name
                    );
                }
                Err(e) => log::warn!(
                    "audio: failed to apply preferred recording device '{}': {e} -- using default",
                    device.name
                ),
            },
            None => log::warn!(
                "audio: preferred recording device {wanted_id} not present -- using default"
            ),
        }
    }

    // `PlatformAudio::new()` already applies `AudioProcessingOptions::default()`
    // internally (see module doc comment) -- called again here, explicitly,
    // purely so the intent (AEC+NS+AGC on) is visible at this call site
    // rather than only inside a dependency's constructor. A no-op in
    // practice since the values match the default already applied.
    if let Err(e) = audio.configure_audio_processing(AudioProcessingOptions::default()) {
        log::warn!("audio: failed to (re-)configure APM options: {e}");
    }

    // #812: the ONLY substitution is the sample source; name/options/publish
    // path below are identical, so a passing AUD-N2W exercises the real
    // publish chain rather than a parallel probe path.
    let capture_source = if synthetic_tone_capture_enabled() {
        synthetic_tone_source()
    } else {
        audio.rtc_source()
    };
    let track = LocalAudioTrack::create_audio_track(MIC_TRACK_NAME, capture_source);
    let publish_options = audio_publish_options();
    let publish_summary = audio_publish_summary(&publish_options);
    // `publish_unmuted_for_tests()` is synth-tone-gated (see its doc comment):
    // a real microphone can never reach this branch.
    if should_mute_before_publish(initial_muted) && !publish_unmuted_for_tests() {
        track.mute();
        log::info!("audio: pre-muted microphone track before publish");
    }

    // Snapshot before publication so the synchronous preparation phase owns
    // every device operation; the async phase below only talks to LiveKit.
    let current_device = if pinned {
        recording_devices
            .iter()
            .find(|d| Some(d.id.as_str()) == preferred.as_deref())
            .map(|d| DeviceSnapshot { id: d.id.clone() })
    } else {
        devices_snapshot(&audio)
    };

    Ok(PreparedMicrophone {
        audio,
        muted: initial_muted && !publish_unmuted_for_tests(),
        track,
        current_device,
        user_pinned: pinned,
        publish_summary,
    })
}

/// Publish an already-prepared microphone. The caller retains `prepared`
/// across this await so a timeout can still mute and best-effort unpublish it.
pub async fn publish_prepared_microphone(
    room: &Arc<Room>,
    prepared: &PreparedMicrophone,
) -> Result<(), AudioError> {
    room.local_participant()
        .publish_track(
            LocalTrack::Audio(prepared.track.clone()),
            audio_publish_options(),
        )
        .await?;

    log::info!(
        "audio: published microphone track ({})",
        prepared.publish_summary
    );
    Ok(())
}

/// Best-effort unpublish after the caller has synchronously muted the retained
/// track. The outer join budget bounds this future to the final cleanup
/// reserve.
pub async fn unpublish_prepared_microphone(
    room: &Arc<Room>,
    prepared: &PreparedMicrophone,
) -> Result<(), AudioError> {
    room.local_participant()
        .unpublish_track(&prepared.track_sid())
        .await?;
    Ok(())
}

fn should_mute_before_publish(initial_muted: bool) -> bool {
    initial_muted
}

fn devices_snapshot(audio: &PlatformAudio) -> Option<DeviceSnapshot> {
    let devices: Vec<_> = audio.recording_devices().collect();
    let first = default_recording_device(&devices)?;
    Some(DeviceSnapshot {
        id: first.id.clone(),
    })
}

// ============================================================================
// Device enumeration + selection commands (issue #28)
// ============================================================================
//
// Rides the exact same `PlatformAudio` surface everything above already
// uses -- zero new crates. `PlatformAudio` is reference-counted over ONE
// process-global ADM (see module doc comment), so a fresh
// `PlatformAudio::new()` inside a command is cheap and -- critically --
// operates on the SAME live ADM the in-room mic/playout handles hold, which
// is what makes `switch_playout_device` from a command affect the live
// call's speaker output.
//
// Preference persistence split (honest boundary): the *durable* store for
// the user's device choice is the frontend session store
// (`src/lib/stores/session.svelte.ts`, localStorage -- the codebase's
// established persistence stand-in). This managed state is only this
// process's in-memory mirror, seeded on startup by the root layout and
// updated by `set_audio_devices`; `join_room` snapshots it and passes the
// selected ids into `publish_microphone`/`enable_managed_playout` at join time so the
// preference applies on the NEXT join too, not just live. Device selections
// made on a transient ADM (no room joined, so no long-lived `PlatformAudio`
// handle alive) do NOT survive the ADM being released -- which is exactly
// why apply-at-join exists.
fn normalize_device_preference(id: Option<String>) -> Option<String> {
    id.filter(|id| !id.is_empty())
}

#[derive(Default)]
pub struct AudioDevicePreferences {
    recording: Mutex<Option<String>>,
    playout: Mutex<Option<String>>,
}

impl AudioDevicePreferences {
    pub(crate) fn recording_device(&self) -> Option<String> {
        self.recording.lock_unpoisoned().clone()
    }

    pub(crate) fn playout_device(&self) -> Option<String> {
        self.playout.lock_unpoisoned().clone()
    }

    pub(crate) fn set_recording_device(&self, id: String) {
        *self.recording.lock_unpoisoned() = normalize_device_preference(Some(id));
    }

    pub(crate) fn set_playout_device(&self, id: String) {
        *self.playout.lock_unpoisoned() = normalize_device_preference(Some(id));
    }
}

/// One enumerated audio device, as sent to the frontend Settings selects.
/// `id` is the stable per-device GUID (`RecordingDeviceId`/`PlayoutDeviceId`
/// -- "stable across device hot-plug events on desktop" per the SDK's own
/// doc comment), NOT the index (which the SDK documents as unstable).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioDeviceInfo {
    pub id: String,
    pub name: String,
}

/// `list_audio_devices`'s payload. Windows prepends an empty-ID "System
/// default" option to each non-empty native recording/playout list.
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AudioDeviceLists {
    pub recording: Vec<AudioDeviceInfo>,
    pub playout: Vec<AudioDeviceInfo>,
}

/// What `set_audio_devices` actually did, so the frontend can render honest
/// state ("switched now" vs. "saved, applies when you join a room" vs. a
/// real error) instead of pretending every selection took effect live.
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AppliedAudioDevices {
    /// True if a live mic track was hot-swapped onto the chosen device.
    pub mic_applied: bool,
    /// True if the live ADM's playout was switched to the chosen device.
    pub speaker_applied: bool,
    /// Whether this process is currently in a room (drives the frontend's
    /// "applies when you join" caption when false).
    pub in_room: bool,
    pub mic_error: Option<String>,
    pub speaker_error: Option<String>,
}

#[cfg(target_os = "windows")]
fn with_system_default(mut devices: Vec<AudioDeviceInfo>) -> Vec<AudioDeviceInfo> {
    if !devices.is_empty() {
        devices.insert(
            0,
            AudioDeviceInfo {
                id: String::new(),
                name: "System default".to_string(),
            },
        );
    }
    devices
}

#[cfg(not(target_os = "windows"))]
fn with_system_default(devices: Vec<AudioDeviceInfo>) -> Vec<AudioDeviceInfo> {
    devices
}

/// Enumerate the machine's real recording + playout devices (issue #28).
/// Errors (no audio hardware, ADM init failure) surface as a string so the
/// frontend can show an honest "audio devices unavailable" state.
#[tauri::command]
pub fn list_audio_devices() -> Result<AudioDeviceLists, String> {
    let audio = PlatformAudio::new().map_err(|e| format!("audio devices unavailable: {e}"))?;
    Ok(AudioDeviceLists {
        recording: with_system_default(
            audio
                .recording_devices()
                .map(|device| AudioDeviceInfo {
                    id: device.id.as_str().to_string(),
                    name: device.name,
                })
                .collect(),
        ),
        playout: with_system_default(
            audio
                .playout_devices()
                .map(|device| AudioDeviceInfo {
                    id: device.id.as_str().to_string(),
                    name: device.name,
                })
                .collect(),
        ),
    })
}

/// Record the user's mic/speaker device choice and apply it live where
/// possible (issue #28). `None` params leave that side's preference
/// untouched. Always records the preference (applied on the next join via
/// `publish_microphone`/`enable_managed_playout`); additionally hot-swaps the live
/// mic track (`MicTrack::set_recording_device`) and/or live playout
/// (`switch_playout_device`) when currently in a room. Per-side failures are
/// reported in the payload, not as a command error -- a dead speaker switch
/// shouldn't discard a valid mic preference recorded in the same call.
#[cfg(target_os = "macos")]
#[tauri::command]
pub fn set_audio_devices(
    recording_id: Option<String>,
    playout_id: Option<String>,
    preferences: tauri::State<'_, AudioDevicePreferences>,
    state: tauri::State<'_, crate::session::SessionState>,
) -> AppliedAudioDevices {
    if let Some(id) = &recording_id {
        preferences.set_recording_device(id.clone());
    }
    if let Some(id) = &playout_id {
        preferences.set_playout_device(id.clone());
    }

    let in_room = state.current_room_name().is_some();
    let mut result = AppliedAudioDevices {
        in_room,
        ..Default::default()
    };
    if !in_room {
        log::info!(
            "audio: device preference saved (mic: {:?}, speaker: {:?}) -- not in a room, applies on next join",
            recording_id.is_some(),
            playout_id.is_some()
        );
        return result;
    }

    if let Some(id) = &recording_id {
        match state.current_mic() {
            Some(mic) => {
                let switched = if id.is_empty() {
                    mic.use_default_recording_device()
                } else {
                    mic.set_recording_device(id)
                };
                match switched {
                    Ok(_) => result.mic_applied = true,
                    Err(error) => result.mic_error = Some(error),
                }
            }
            // In a room but no live mic track (mic publish failed at join,
            // or PETAL_DISABLE_AUDIO run) -- honest error, the preference is
            // still recorded for the next join.
            None => result.mic_error = Some("no live microphone track".to_string()),
        }
    }

    if let Some(id) = &playout_id {
        // In-room, so `session.rs` holds a live `PlatformAudio` playout
        // handle -- this `new()` joins the same ref-counted ADM and the
        // switch affects the live call's speaker output.
        match PlatformAudio::new() {
            Ok(audio) => {
                let device = if id.is_empty() {
                    audio.playout_devices().next()
                } else {
                    audio
                        .playout_devices()
                        .find(|device| device.id.as_str() == id.as_str())
                };
                match device {
                    Some(device) => match audio.switch_playout_device(&device.id) {
                        Ok(()) => {
                            result.speaker_applied = true;
                            log::info!("audio: playout hot-swapped to '{}'", device.name);
                        }
                        Err(error) => {
                            result.speaker_error =
                                Some(format!("failed to switch playout device: {error}"));
                        }
                    },
                    None => {
                        result.speaker_error = Some(if id.is_empty() {
                            "no playout devices available".to_string()
                        } else {
                            format!("playout device not found: {id}")
                        })
                    }
                }
            }
            Err(error) => {
                result.speaker_error = Some(format!("audio devices unavailable: {error}"));
            }
        }
    }

    result
}

#[cfg(not(target_os = "macos"))]
#[tauri::command]
pub async fn set_audio_devices(
    recording_id: Option<String>,
    playout_id: Option<String>,
    preferences: tauri::State<'_, AudioDevicePreferences>,
    state: tauri::State<'_, crate::session::SessionState>,
) -> Result<AppliedAudioDevices, String> {
    Ok(state
        .set_audio_devices(recording_id, playout_id, preferences.inner())
        .await)
}

/// Start a background task that logs every remote audio track `room`
/// subscribes to AND watches each one for produced output, so playback is
/// visibly confirmed (not just "assumed to work because nothing errored") the
/// same way `publisher.rs`'s `log_encoder_once` confirms the video encoder
/// rather than trusting a silently-ignored preference.
///
/// Playback itself needs no code here (see `enable_managed_playout`'s doc comment)
/// -- this observes `RoomEvent::TrackSubscribed` for `RemoteTrack::Audio`,
/// logs identifying info read back from the live track, and hands the track
/// to `watch_remote_audio_track` (#787, see the watchdog section below) so a
/// subscribed-but-producing-nothing track is an alarmed state rather than an
/// invisible one.
///
/// `RoomEvent::ActiveSpeakersChanged` is tracked on the same loop because the
/// watchdog needs it: "we decoded silence" is only a fault when the SFU says
/// that participant is actually speaking. The SFU computes active speakers
/// from the publisher's RTP audio-level header extension, which is
/// independent of whether *our* decoder produced anything -- that
/// independence is what makes it a usable positive control.
///
/// A second, independent `room.subscribe()` receiver -- exactly like
/// `telepointer::start_receiver_for_room`'s own independent subscription to
/// the same room's events alongside this one and the video-focused
/// `Subscriber` in `subscriber.rs`. `Room::subscribe()` fans out to as many
/// independent receivers as callers create (confirmed by this codebase
/// already doing it twice for the same room -- telepointer data + this), so
/// there's no need to fold this into another module's loop.
pub(crate) fn start_audio_track_logger(room: Arc<Room>, generation: RoomGeneration) {
    let mut events = room.subscribe();
    let speaking: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    tokio::spawn(async move {
        // #787: audio tracks that were ALREADY publishing when we joined are
        // auto-subscribed during connect, so their `TrackSubscribed` fired
        // before this logger existed -- the exact ordering of the live
        // incident, and it left both this log line and the watchdog blind.
        // Enumerate them at start; `watched` dedupes against a late event.
        let mut watched: HashSet<String> = HashSet::new();
        for (_, participant) in room.remote_participants() {
            for publication in participant.track_publications().values() {
                if let Some(RemoteTrack::Audio(audio_track)) = publication.track() {
                    let sid = audio_track.sid().to_string();
                    if watched.insert(sid) {
                        log::info!(
                            "audio: subscribed to remote audio track from '{}' (sid={}, muted={}) -- pre-existing at join",
                            participant.identity(),
                            audio_track.sid(),
                            audio_track.is_muted()
                        );
                        let identity = participant.identity().to_string();
                        let speaking = speaking.clone();
                        let generation = generation.clone();
                        tokio::spawn(async move {
                            watch_remote_audio_track(identity, audio_track, speaking, generation)
                                .await;
                        });
                    }
                }
            }
        }
        while let Some(event) = events.recv().await {
            if !generation.is_current() {
                log::debug!("audio: remote track logger exiting for stale room generation");
                break;
            }
            match event {
                RoomEvent::TrackSubscribed {
                    track, participant, ..
                } => {
                    if let RemoteTrack::Audio(audio_track) = track {
                        if !watched.insert(audio_track.sid().to_string()) {
                            continue;
                        }
                        log::info!(
                            "audio: subscribed to remote audio track from '{}' (sid={}, muted={})",
                            participant.identity(),
                            audio_track.sid(),
                            audio_track.is_muted()
                        );
                        let identity = participant.identity().to_string();
                        let speaking = speaking.clone();
                        let generation = generation.clone();
                        tokio::spawn(async move {
                            watch_remote_audio_track(identity, audio_track, speaking, generation)
                                .await;
                        });
                    }
                }
                RoomEvent::TrackUnsubscribed { publication, .. } => {
                    // #787 (Fable review): without this, a LiveKit resume
                    // that re-subscribes the SAME sid within one room
                    // generation would be deduped into permanent blindness
                    // -- recreating the exact gap this logger closes.
                    watched.remove(publication.sid().to_string().as_str());
                }
                RoomEvent::ActiveSpeakersChanged { speakers } => {
                    let mut guard = speaking.lock_unpoisoned();
                    guard.clear();
                    for speaker in speakers {
                        guard.insert(speaker.identity().to_string());
                    }
                }
                _ => {}
            }
        }
    });
}

// ============================================================================
// Receive-side audio watchdog (#787)
// ============================================================================
//
// COURSE_CORRECTION §4c.4 -- "liveness is not throughput". Before this, a
// subscribed remote audio track logged one cheerful `subscribed to remote
// audio track` line and nothing else, for the whole meeting, whether the
// decoder was producing speech or producing nothing at all. #787 is exactly
// that gap: a native listener heard a web participant not at all, every
// health signal green, for an entire call. So the "alive" signal now has a
// produced-output counter beside it, and *alive with zero output* is its own
// alarmed state.
//
// WHAT THIS CAN AND CANNOT SEE -- read before drawing a conclusion from it.
// It taps the decoded PCM that WebRTC's audio sink hands us. Those sinks are
// pulled by the audio device module, and the synthetic null ADM
// (`vendor/webrtc-sys/src/synthetic_audio_device.cpp`) pulls on the same
// 10 ms cadence a real device would -- so audio that is decoded and then
// discarded into the null sink still reads as `Audible` here. That is
// deliberate: it makes this verdict the A-vs-B discriminator #787 asks for.
//   `Audible` + user hears nothing  => playout/ADM side (#787 hypothesis A).
//   `NoDecodedFrames`/`SilentWhileSpeaking` => decode side (hypothesis B).
// Do NOT read `Audible` as "the user heard it".

/// Decode the watchdog tap at 16 kHz mono. Nothing here needs fidelity --
/// only "is there energy" -- and a low rate keeps the per-frame work to a
/// short scan over ~160 samples.
const WATCHDOG_SAMPLE_RATE: i32 = 16_000;
const WATCHDOG_CHANNELS: i32 = 1;

/// One evaluation window. Long enough that an ordinary conversational pause
/// cannot trip it, short enough that a dead call is named while the meeting
/// is still happening.
const WATCHDOG_WINDOW: Duration = Duration::from_secs(10);

/// |sample| at or above this counts as real content. ~-54 dBFS of a full
/// i16: comfortably under any speech or the cockpit's 0.15-gain 440 Hz tone
/// (peak ~4915), comfortably over digital silence and low-level comfort
/// noise.
pub(crate) const AUDIBLE_PEAK_FLOOR: u16 = 64;

/// How many audible 10 ms frames a window needs before it counts as audible.
/// A single stray frame over the floor must not be able to launder a dead
/// track into a healthy verdict; 5 frames is 50 ms of real content.
const AUDIBLE_FRAMES_MIN: u64 = 5;

/// Peak |sample| in a decoded PCM buffer. `i16::MIN` is why this returns
/// `u16` -- `(-32768i16).abs()` overflows, `unsigned_abs()` does not.
pub(crate) fn pcm_peak_abs(samples: &[i16]) -> u16 {
    samples
        .iter()
        .map(|sample| sample.unsigned_abs())
        .max()
        .unwrap_or(0)
}

pub(crate) fn pcm_is_audible(samples: &[i16]) -> bool {
    pcm_peak_abs(samples) >= AUDIBLE_PEAK_FLOOR
}

/// Energy readout for a captured stretch of decoded PCM. Its only production
/// consumer is the test cockpit's `AUD` oracle (`cockpit-privileged` feature),
/// so a default build sees it as dead -- hence the scoped `allow`, rather than
/// leaving a warning for everyone or hiding a real one behind a blanket
/// attribute.
#[cfg_attr(not(feature = "cockpit-privileged"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DecodedPcmEnergy {
    pub samples: usize,
    pub peak_abs: u16,
    pub nonzero_samples: usize,
}

#[cfg_attr(not(feature = "cockpit-privileged"), allow(dead_code))]
impl DecodedPcmEnergy {
    pub(crate) fn is_audible(&self) -> bool {
        self.peak_abs >= AUDIBLE_PEAK_FLOOR
    }

    pub(crate) fn summary(&self) -> String {
        format!(
            "samples={} peak_abs={} nonzero={} floor={}",
            self.samples, self.peak_abs, self.nonzero_samples, AUDIBLE_PEAK_FLOOR
        )
    }
}

#[cfg_attr(not(feature = "cockpit-privileged"), allow(dead_code))]
pub(crate) fn decoded_pcm_energy(samples: &[i16]) -> DecodedPcmEnergy {
    DecodedPcmEnergy {
        samples: samples.len(),
        peak_abs: pcm_peak_abs(samples),
        nonzero_samples: samples.iter().filter(|sample| **sample != 0).count(),
    }
}

/// What one window of a subscribed remote audio track produced. Plain data
/// on purpose: `classify_remote_audio` is a pure function over this, so it is
/// testable without a room, a device, or an event loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RemoteAudioWindow {
    pub elapsed: Duration,
    /// Decoded frames the sink delivered in this window.
    pub frames: u64,
    /// Of those, how many cleared `AUDIBLE_PEAK_FLOOR`.
    pub audible_frames: u64,
    /// The remote publisher had this track muted at evaluation time.
    pub track_muted: bool,
    /// The SFU reported this participant as an active speaker during the
    /// window. Derived from the publisher's RTP audio-level extension, so it
    /// is independent of our own decode.
    pub remote_speaking: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteAudioVerdict {
    /// Too little elapsed time to judge yet.
    Warming,
    /// Remote muted the track -- silence is the correct outcome.
    Muted,
    /// Decoded PCM carried real energy. NOT proof the user heard it.
    Audible,
    /// Frames flowed, all below the floor, and nobody claimed to be
    /// speaking. The ordinary "nobody is talking" state.
    IdleSilence,
    /// No decoded frames reached the sink at all. Either nothing is pulling
    /// the audio mixer (no playout of any kind) or the track never decodes.
    NoDecodedFrames,
    /// Frames flowed but carried nothing, while the SFU said this
    /// participant was speaking. The strongest available evidence that a
    /// listener is being denied audio someone is actually producing.
    SilentWhileSpeaking,
}

impl RemoteAudioVerdict {
    pub(crate) fn is_alarmed(self) -> bool {
        matches!(self, Self::NoDecodedFrames | Self::SilentWhileSpeaking)
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Warming => "warming",
            Self::Muted => "muted",
            Self::Audible => "audible",
            Self::IdleSilence => "idle-silence",
            Self::NoDecodedFrames => "no-decoded-frames",
            Self::SilentWhileSpeaking => "silent-while-speaking",
        }
    }
}

/// The whole decision, as a pure function. Order is load-bearing:
///
/// - `Audible` outranks `Muted` so a mute flag that arrives mid-window
///   cannot suppress a window that genuinely carried speech.
/// - `Muted` outranks both alarms: a muted publisher producing nothing is
///   correct behavior, not a fault, and alarming on it would be the kind of
///   check that fires on healthy states and gets ignored.
/// - `NoDecodedFrames` does not need `remote_speaking`. Zero frames over a
///   whole window is not "nobody talked": something pulls the mixer every
///   10 ms whenever playout exists at all, so zero means the pull or the
///   decode is dead regardless of who is speaking.
pub(crate) fn classify_remote_audio(
    window: RemoteAudioWindow,
    min_window: Duration,
    audible_frames_min: u64,
) -> RemoteAudioVerdict {
    if window.elapsed < min_window {
        return RemoteAudioVerdict::Warming;
    }
    if window.audible_frames >= audible_frames_min {
        return RemoteAudioVerdict::Audible;
    }
    if window.track_muted {
        return RemoteAudioVerdict::Muted;
    }
    if window.frames == 0 {
        return RemoteAudioVerdict::NoDecodedFrames;
    }
    if window.remote_speaking {
        return RemoteAudioVerdict::SilentWhileSpeaking;
    }
    RemoteAudioVerdict::IdleSilence
}

/// What to actually emit for a verdict, given the previous one. Pure so the
/// "log the alarm once, not every 10 s, and say so when it clears" behavior
/// is testable -- a watchdog that re-screams every window is one people mute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WatchdogReport {
    /// First real verdict for this track: worth one info line so a normal
    /// log shows the produced-output counter, not just the subscribe.
    FirstVerdict,
    /// Newly alarmed. `log::warn!` -- this is a quality watchdog, not a
    /// crash. error! would open a Sentry issue per track; warn stays in
    /// petal.log and only becomes a breadcrumb if something else errors.
    EnteredAlarm,
    /// Was alarmed, now is not.
    RecoveredFromAlarm,
    /// Steady state -- debug only.
    Unchanged,
}

pub(crate) fn watchdog_report(
    previous: Option<RemoteAudioVerdict>,
    current: RemoteAudioVerdict,
) -> WatchdogReport {
    // `Warming` is not a verdict, it is the absence of one -- and the first
    // window can legitimately land there, because `interval` starts ticking a
    // hair before the window's own start instant. Letting it count would spend
    // the single `FirstVerdict` info line on a non-answer and demote the real
    // first verdict to `debug`, which is precisely the produced-output counter
    // this whole thing exists to make visible. The caller must likewise not
    // store `Warming` as `previous` (see `watch_remote_audio_track`).
    if matches!(current, RemoteAudioVerdict::Warming) {
        return WatchdogReport::Unchanged;
    }
    match previous {
        None => {
            if current.is_alarmed() {
                WatchdogReport::EnteredAlarm
            } else {
                WatchdogReport::FirstVerdict
            }
        }
        Some(previous) => {
            if current.is_alarmed() && !previous.is_alarmed() {
                WatchdogReport::EnteredAlarm
            } else if previous.is_alarmed() && !current.is_alarmed() {
                WatchdogReport::RecoveredFromAlarm
            } else {
                WatchdogReport::Unchanged
            }
        }
    }
}

/// Tap one subscribed remote audio track's decoded PCM and report each
/// window's verdict. Ends when the track's stream ends (unsubscribe/leave)
/// or the room generation goes stale.
async fn watch_remote_audio_track(
    identity: String,
    track: RemoteAudioTrack,
    speaking: Arc<Mutex<HashSet<String>>>,
    generation: RoomGeneration,
) {
    let sid = track.sid();
    let mut stream =
        NativeAudioStream::new(track.rtc_track(), WATCHDOG_SAMPLE_RATE, WATCHDOG_CHANNELS);
    let mut ticker = tokio::time::interval(WATCHDOG_WINDOW);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // `interval`'s first tick completes immediately; burn it so the first
    // real evaluation covers a full window rather than zero elapsed time.
    ticker.tick().await;

    let mut window_started = tokio::time::Instant::now();
    let mut frames: u64 = 0;
    let mut audible_frames: u64 = 0;
    let mut window_peak_abs: u16 = 0;
    let mut speaking_seen = false;
    let mut previous: Option<RemoteAudioVerdict> = None;

    loop {
        tokio::select! {
            frame = stream.next() => {
                let Some(frame) = frame else { break };
                frames += 1;
                let peak = pcm_peak_abs(&frame.data);
                window_peak_abs = window_peak_abs.max(peak);
                if peak >= AUDIBLE_PEAK_FLOOR {
                    audible_frames += 1;
                }
                if !speaking_seen && speaking.lock_unpoisoned().contains(&identity) {
                    speaking_seen = true;
                }
            }
            _ = ticker.tick() => {
                if !generation.is_current() {
                    break;
                }
                let window = RemoteAudioWindow {
                    elapsed: window_started.elapsed(),
                    frames,
                    audible_frames,
                    track_muted: track.is_muted(),
                    remote_speaking: speaking_seen
                        || speaking.lock_unpoisoned().contains(&identity),
                };
                let verdict = classify_remote_audio(window, WATCHDOG_WINDOW, AUDIBLE_FRAMES_MIN);
                let detail = format!(
                    "audio: remote track from '{}' (sid={}) -- {} over {:?} (frames={}, audible_frames={}, peak_abs={} floor={}, muted={}, sfu_says_speaking={})",
                    crate::logging::log_safe_quoted(&identity),
                    sid,
                    verdict.label(),
                    window.elapsed,
                    window.frames,
                    window.audible_frames,
                    window_peak_abs,
                    AUDIBLE_PEAK_FLOOR,
                    window.track_muted,
                    window.remote_speaking,
                );
                match watchdog_report(previous, verdict) {
                    WatchdogReport::EnteredAlarm => {
                        log::warn!(
                        "{detail} -- SUBSCRIBED BUT PRODUCING NO AUDIBLE PCM (#787). What this \
                         narrows to: decode, or a playout pump that never started. NOT a wrong \
                         or muted output device -- audio decoded and then discarded into the \
                         synthetic null ADM pulls sinks identically to a real one and would read \
                         'audible' here instead."
                        );
                        crate::analytics::remote_audio_silent(WATCHDOG_WINDOW);
                    }
                    WatchdogReport::RecoveredFromAlarm => {
                        log::info!("{detail} -- recovered, remote audio is producing output again")
                    }
                    WatchdogReport::FirstVerdict => log::info!("{detail}"),
                    WatchdogReport::Unchanged => log::debug!("{detail}"),
                }
                if !matches!(verdict, RemoteAudioVerdict::Warming) {
                    previous = Some(verdict);
                }
                frames = 0;
                audible_frames = 0;
                window_peak_abs = 0;
                speaking_seen = false;
                window_started = tokio::time::Instant::now();
            }
        }
    }

    log::info!(
        "audio: remote audio watchdog for '{}' (sid={}) ended",
        crate::logging::log_safe_quoted(&identity),
        sid
    );
}

#[cfg(test)]
mod tests {
    /// Env-var tests mutate process-global state; serialize them so a sibling
    /// test cannot observe a half-set pair (mutation-check trap: a racing test
    /// writing the same globals can rescue a broken guard).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// #821: the unmute hook must be unreachable without the synthetic tone.
    /// Un-gated, a leaked `PETAL_AUDIO_PUBLISH_UNMUTED=1` in a dev shell joins
    /// a REAL microphone hot and makes the mute button a silent no-op while
    /// the UI still reads muted. The env pair is the safety property, so it is
    /// pinned here rather than left to the doc comment.
    #[test]
    fn publish_unmuted_hook_requires_the_synthetic_tone() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let restore = (
            std::env::var("PETAL_AUDIO_SYNTH_TONE").ok(),
            std::env::var("PETAL_AUDIO_PUBLISH_UNMUTED").ok(),
        );

        std::env::set_var("PETAL_AUDIO_PUBLISH_UNMUTED", "1");
        std::env::remove_var("PETAL_AUDIO_SYNTH_TONE");
        assert!(
            !publish_unmuted_for_tests(),
            "the unmute hook must be inert without PETAL_AUDIO_SYNTH_TONE=1 -- a real mic must never join unmuted"
        );

        std::env::set_var("PETAL_AUDIO_SYNTH_TONE", "1");
        assert!(
            publish_unmuted_for_tests(),
            "with the synthetic tone the hook must work, or audio rigs measure a muted track"
        );

        match restore.0 {
            Some(value) => std::env::set_var("PETAL_AUDIO_SYNTH_TONE", value),
            None => std::env::remove_var("PETAL_AUDIO_SYNTH_TONE"),
        }
        match restore.1 {
            Some(value) => std::env::set_var("PETAL_AUDIO_PUBLISH_UNMUTED", value),
            None => std::env::remove_var("PETAL_AUDIO_PUBLISH_UNMUTED"),
        }
    }

    /// #812: `=0` must ENABLE audio. Every doc and the cockpit launcher tell
    /// users to write `PETAL_DISABLE_AUDIO=0` for an audio run; when this
    /// returned true for "0", those runs published no mic at all.
    #[test]
    fn audio_disable_flag_treats_zero_as_enabled() {
        assert!(!audio_is_disabled(Some("0")));
        assert!(!audio_is_disabled(Some("false")));
        assert!(!audio_is_disabled(Some("off")));
        assert!(!audio_is_disabled(Some(" 0 ")));
        assert!(!audio_is_disabled(None));
        assert!(!audio_is_disabled(Some("")));
        assert!(audio_is_disabled(Some("1")));
        assert!(audio_is_disabled(Some("true")));
        assert!(audio_is_disabled(Some("yes")));
    }

    use super::*;

    // Same lesson as `resilience.rs`'s own serde tests (which caught a real
    // `rename_all` vs `rename_all_fields` bug): assert the exact wire field
    // names the frontend (`src/lib/data/audioDevices.ts`) matches on, so a
    // silent casing regression can't ship.
    #[test]
    fn audio_device_lists_serialize_camel_case() {
        let lists = AudioDeviceLists {
            recording: vec![AudioDeviceInfo {
                id: "guid-1".into(),
                name: "Mic".into(),
            }],
            playout: vec![AudioDeviceInfo {
                id: "guid-2".into(),
                name: "Speakers".into(),
            }],
        };
        let json = serde_json::to_value(&lists).unwrap();
        assert_eq!(json["recording"][0]["id"], "guid-1");
        assert_eq!(json["recording"][0]["name"], "Mic");
        assert_eq!(json["playout"][0]["id"], "guid-2");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_device_lists_prepend_system_default() {
        let devices = with_system_default(vec![AudioDeviceInfo {
            id: "guid-1".into(),
            name: "Mic".into(),
        }]);

        assert_eq!(devices[0].id, "");
        assert_eq!(devices[0].name, "System default");
        assert_eq!(devices[1].id, "guid-1");
        assert!(with_system_default(Vec::new()).is_empty());
    }

    #[test]
    fn applied_audio_devices_serialize_camel_case() {
        let applied = AppliedAudioDevices {
            mic_applied: true,
            speaker_applied: false,
            in_room: true,
            mic_error: None,
            speaker_error: Some("nope".into()),
        };
        let json = serde_json::to_value(&applied).unwrap();
        assert_eq!(json["micApplied"], true);
        assert_eq!(json["speakerApplied"], false);
        assert_eq!(json["inRoom"], true);
        assert!(json["micError"].is_null());
        assert_eq!(json["speakerError"], "nope");
    }

    #[test]
    fn managed_preferences_round_trip() {
        let preferences = AudioDevicePreferences::default();

        preferences.set_recording_device("rec-guid".into());
        preferences.set_playout_device("play-guid".into());

        assert_eq!(preferences.recording_device().as_deref(), Some("rec-guid"));
        assert_eq!(preferences.playout_device().as_deref(), Some("play-guid"));
    }

    #[test]
    fn empty_device_id_selects_system_default() {
        assert_eq!(normalize_device_preference(Some(String::new())), None);
        assert_eq!(normalize_device_preference(None), None);
        assert_eq!(
            normalize_device_preference(Some("device-guid".into())).as_deref(),
            Some("device-guid")
        );
    }

    #[test]
    fn default_join_intent_pre_mutes_track_before_publish() {
        assert!(should_mute_before_publish(true));
        assert!(!should_mute_before_publish(false));
    }

    /// #787: `MIC_TRACK_NAME`'s doc comment claimed `petal-mic` was in
    /// `docs/CONTRACTS.md`. It was not, and nothing pinned the two sides
    /// together -- native and web each hard-coded the literal independently.
    /// Now both are pinned to the shared fixture (the web half lives in
    /// `web-harness/tests/contracts.test.ts`).
    #[test]
    fn mic_track_name_matches_the_shared_native_web_fixture() {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct MicTrackFixture {
            track_name: String,
            source: String,
        }
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct ContractFixture {
            mic_track: MicTrackFixture,
        }

        let fixture: ContractFixture =
            // Five levels, not four: this file is `src/transport/`, one deeper
            // than the `src/*.rs` fixtures that use four (see
            // `room_directory.rs:116`, the correct sibling in this directory).
            serde_json::from_str(include_str!("../../../../../contracts/petal-contracts.json"))
                .expect("shared contract fixture parses");
        assert_eq!(MIC_TRACK_NAME, fixture.mic_track.track_name);
        assert_eq!(fixture.mic_track.source, "microphone");
        assert_eq!(audio_publish_options().source, TrackSource::Microphone);
    }

    // Regression pin for the one-way web/mobile audio bug: RED must stay off
    // (`TrackPublishOptions::default()` has `red: true`) -- see module doc
    // comment's "RED" section for why blanket RED silently breaks decode on
    // some browser/mobile subscribers while desktop-to-desktop looks fine.
    #[test]
    fn audio_publish_options_disables_red() {
        let options = audio_publish_options();
        assert!(!options.red, "RED must be disabled for subscriber interop");
        assert!(options.dtx, "DTX should remain on per SPEC.md §4.9");
    }

    // ------------------------------------------------------------------
    // #787 receive-side watchdog. Every audio test that existed before this
    // was publish-side option/serialization checking -- nothing covered the
    // receive path, which is precisely why a whole-meeting silent failure
    // shipped green.
    // ------------------------------------------------------------------

    fn window(frames: u64, audible_frames: u64) -> RemoteAudioWindow {
        RemoteAudioWindow {
            elapsed: WATCHDOG_WINDOW,
            frames,
            audible_frames,
            track_muted: false,
            remote_speaking: false,
        }
    }

    fn classify(window: RemoteAudioWindow) -> RemoteAudioVerdict {
        classify_remote_audio(window, WATCHDOG_WINDOW, AUDIBLE_FRAMES_MIN)
    }

    #[test]
    fn peak_abs_survives_i16_min() {
        // `(-32768i16).abs()` panics in debug / overflows in release; a
        // watchdog that panics on a legal sample value is worse than no
        // watchdog.
        assert_eq!(pcm_peak_abs(&[i16::MIN]), 32_768);
        assert_eq!(pcm_peak_abs(&[]), 0);
        assert_eq!(pcm_peak_abs(&[0, -3, 5]), 5);
    }

    #[test]
    fn digital_silence_is_never_audible_and_speech_level_always_is() {
        assert!(!pcm_is_audible(&[0; 160]));
        assert!(!pcm_is_audible(&[])); // no frames at all
                                       // Low-level comfort noise stays under the floor...
        assert!(!pcm_is_audible(&[8, -12, 3, 63]));
        // ...while anything at conversational level clears it.
        assert!(pcm_is_audible(&[0, 0, 4_915]));
        assert!(pcm_is_audible(&[i16::MIN]));
    }

    #[test]
    fn decoded_pcm_energy_separates_silence_from_a_tone() {
        let silence = decoded_pcm_energy(&[0; 480]);
        assert_eq!(silence.samples, 480);
        assert_eq!(silence.peak_abs, 0);
        assert_eq!(silence.nonzero_samples, 0);
        assert!(!silence.is_audible());

        // A 440 Hz sine at the harness's 0.15 gain, sampled at 48 kHz.
        let tone: Vec<i16> = (0..480)
            .map(|n| {
                let t = n as f64 / 48_000.0;
                (0.15 * (2.0 * std::f64::consts::PI * 440.0 * t).sin() * i16::MAX as f64) as i16
            })
            .collect();
        let energy = decoded_pcm_energy(&tone);
        assert!(energy.is_audible(), "{}", energy.summary());
        assert!(energy.peak_abs > AUDIBLE_PEAK_FLOOR);
        assert!(energy.nonzero_samples > 400);
    }

    #[test]
    fn a_subscribed_track_that_decodes_nothing_is_alarmed() {
        // The #787 shape: subscribed, telemetry green, zero output.
        assert_eq!(classify(window(0, 0)), RemoteAudioVerdict::NoDecodedFrames);
        assert!(RemoteAudioVerdict::NoDecodedFrames.is_alarmed());
    }

    #[test]
    fn frames_that_carry_nothing_alarm_only_when_someone_is_speaking() {
        let quiet = window(1_000, 0);
        // Nobody is talking: silence is the correct output, not a fault.
        assert_eq!(classify(quiet), RemoteAudioVerdict::IdleSilence);
        assert!(!RemoteAudioVerdict::IdleSilence.is_alarmed());

        let speaking = RemoteAudioWindow {
            remote_speaking: true,
            ..quiet
        };
        assert_eq!(classify(speaking), RemoteAudioVerdict::SilentWhileSpeaking);
        assert!(RemoteAudioVerdict::SilentWhileSpeaking.is_alarmed());
    }

    #[test]
    fn a_muted_remote_never_alarms() {
        let muted = RemoteAudioWindow {
            track_muted: true,
            remote_speaking: true,
            ..window(0, 0)
        };
        assert_eq!(classify(muted), RemoteAudioVerdict::Muted);
        assert!(!RemoteAudioVerdict::Muted.is_alarmed());
    }

    #[test]
    fn one_stray_loud_frame_cannot_launder_a_dead_track() {
        assert_eq!(
            classify(window(1_000, AUDIBLE_FRAMES_MIN - 1)),
            RemoteAudioVerdict::IdleSilence
        );
        assert_eq!(
            classify(window(1_000, AUDIBLE_FRAMES_MIN)),
            RemoteAudioVerdict::Audible
        );
    }

    #[test]
    fn audible_outranks_a_mute_flag_that_lands_mid_window() {
        let muted_but_loud = RemoteAudioWindow {
            track_muted: true,
            ..window(1_000, 50)
        };
        assert_eq!(classify(muted_but_loud), RemoteAudioVerdict::Audible);
    }

    #[test]
    fn a_short_window_is_never_a_verdict() {
        let early = RemoteAudioWindow {
            elapsed: WATCHDOG_WINDOW - Duration::from_millis(1),
            ..window(0, 0)
        };
        assert_eq!(classify(early), RemoteAudioVerdict::Warming);
        assert!(!RemoteAudioVerdict::Warming.is_alarmed());
    }

    #[test]
    fn the_alarm_fires_once_and_says_so_when_it_clears() {
        // A watchdog that re-screams every 10s is one people learn to
        // ignore. The alarm itself is warn! (petal.log), not error!
        // (Sentry issue).
        assert_eq!(
            watchdog_report(None, RemoteAudioVerdict::Audible),
            WatchdogReport::FirstVerdict
        );
        assert_eq!(
            watchdog_report(None, RemoteAudioVerdict::NoDecodedFrames),
            WatchdogReport::EnteredAlarm
        );
        assert_eq!(
            watchdog_report(
                Some(RemoteAudioVerdict::IdleSilence),
                RemoteAudioVerdict::NoDecodedFrames
            ),
            WatchdogReport::EnteredAlarm
        );
        assert_eq!(
            watchdog_report(
                Some(RemoteAudioVerdict::NoDecodedFrames),
                RemoteAudioVerdict::NoDecodedFrames
            ),
            WatchdogReport::Unchanged
        );
        // Alarm -> different alarm stays Unchanged: still broken, already said.
        assert_eq!(
            watchdog_report(
                Some(RemoteAudioVerdict::NoDecodedFrames),
                RemoteAudioVerdict::SilentWhileSpeaking
            ),
            WatchdogReport::Unchanged
        );
        assert_eq!(
            watchdog_report(
                Some(RemoteAudioVerdict::NoDecodedFrames),
                RemoteAudioVerdict::Audible
            ),
            WatchdogReport::RecoveredFromAlarm
        );
    }

    /// The first window can land on `Warming` (the interval starts ticking a
    /// hair before the window's own start instant). If that counted as a
    /// verdict it would consume the one `FirstVerdict` info line and push the
    /// real answer down to `debug` -- i.e. the produced-output counter would
    /// be invisible at the default log level, which is the exact failure mode
    /// (#787 / COURSE_CORRECTION §4.2) this is supposed to end.
    #[test]
    fn warming_never_consumes_the_first_verdict_line() {
        assert_eq!(
            watchdog_report(None, RemoteAudioVerdict::Warming),
            WatchdogReport::Unchanged
        );
        // ...and, because the caller must not store it, the next real verdict
        // still sees `None` and reports as the first.
        assert_eq!(
            watchdog_report(None, RemoteAudioVerdict::Audible),
            WatchdogReport::FirstVerdict
        );
        // A warming window mid-stream must not read as a recovery either.
        assert_eq!(
            watchdog_report(
                Some(RemoteAudioVerdict::NoDecodedFrames),
                RemoteAudioVerdict::Warming
            ),
            WatchdogReport::Unchanged
        );
    }

    // ---- Positive controls: each gate must be able to FAIL. ----
    //
    // COURSE_CORRECTION §4b: a "could not reproduce" is worthless unless the
    // instrument is known capable of detecting the failure. The `AUD` oracle
    // these back: returned PASS for a silent call, so the replacement's
    // ability to reject silence is the entire point of it existing.

    #[test]
    fn positive_control_the_energy_gate_rejects_the_exact_pre_fix_failure() {
        // What #787's native listener actually got: a subscribed track whose
        // decoded PCM is digital silence. The OLD oracle asserted
        // `actual_kbps > 0.0 || stream_state == "active"` -- both true here.
        let three_seconds_of_silence = vec![0i16; 48_000 * 3];
        let energy = decoded_pcm_energy(&three_seconds_of_silence);
        assert!(
            !energy.is_audible(),
            "the gate must reject a silent call: {}",
            energy.summary()
        );

        // And it must still accept a real one, or it is just a broken gate.
        let tone: Vec<i16> = (0..48_000 * 3)
            .map(|n| {
                let t = n as f64 / 48_000.0;
                (0.15 * (2.0 * std::f64::consts::PI * 440.0 * t).sin() * i16::MAX as f64) as i16
            })
            .collect();
        assert!(decoded_pcm_energy(&tone).is_audible());
    }

    #[test]
    fn positive_control_the_watchdog_alarms_on_the_reported_incident_shape() {
        // Reported shape: native subscribed to the web participant's mic,
        // that participant was talking, native heard nothing.
        let reported = RemoteAudioWindow {
            elapsed: WATCHDOG_WINDOW,
            frames: 1_000,
            audible_frames: 0,
            track_muted: false,
            remote_speaking: true,
        };
        assert!(classify(reported).is_alarmed());
        assert_eq!(
            watchdog_report(Some(RemoteAudioVerdict::Warming), classify(reported)),
            WatchdogReport::EnteredAlarm
        );

        // The same window with the remote actually silent must NOT alarm --
        // otherwise the alarm fires all meeting and means nothing.
        let nobody_talking = RemoteAudioWindow {
            remote_speaking: false,
            ..reported
        };
        assert!(!classify(nobody_talking).is_alarmed());
    }

    #[test]
    fn verdict_labels_are_all_distinct_and_stable() {
        // These land in petal.log; a duplicate or renamed label breaks
        // grep-based triage.
        let labels: Vec<&str> = [
            RemoteAudioVerdict::Warming,
            RemoteAudioVerdict::Muted,
            RemoteAudioVerdict::Audible,
            RemoteAudioVerdict::IdleSilence,
            RemoteAudioVerdict::NoDecodedFrames,
            RemoteAudioVerdict::SilentWhileSpeaking,
        ]
        .iter()
        .map(|verdict| verdict.label())
        .collect();
        let mut unique = labels.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), labels.len(), "labels must be distinct");
        assert_eq!(
            labels,
            vec![
                "warming",
                "muted",
                "audible",
                "idle-silence",
                "no-decoded-frames",
                "silent-while-speaking"
            ]
        );
    }

    #[test]
    fn audio_publish_summary_reflects_actual_options() {
        let options = audio_publish_options();
        assert_eq!(
            audio_publish_summary(&options),
            "Opus, DTX on, RED off, in-band FEC always-on"
        );

        let inverse = TrackPublishOptions {
            dtx: false,
            red: true,
            ..Default::default()
        };
        assert_eq!(
            audio_publish_summary(&inverse),
            "Opus, DTX off, RED on, in-band FEC always-on"
        );
    }
}
