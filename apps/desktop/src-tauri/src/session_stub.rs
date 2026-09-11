//! Windows room-membership and native media session.
//!
//! Joining, leaving, current-room state, LiveKit presence, and the retained
//! connection are portable. Windows attaches WASAPI microphone publication,
//! remote playout, Media Foundation camera publication, and WGC window/display
//! share publication to that connection; the receiver compositor and the
//! share feed live in `windows_compositor`/`transport::subscriber`.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};

use crate::remote_control_core::RemoteControlPolicy;
use std::sync::{Arc, Mutex};

use crate::camera_session::ActiveCamera;
// Re-exported so `crate::session::RoomGeneration` keeps working for the many
// external consumers that imported it through the session module.
pub(crate) use crate::room_generation::RoomGeneration;
use crate::screen_audio::{AudioSourceKey, AudioSourceRegistry, AudioSourceTransition};
use crate::sync_ext::MutexExt;
use crate::transport::audio::ScreenAudioTrack;
use crate::transport::publisher::{RoomConnection, SharedSourceKind};
use crate::video_color::VideoColorProfile;
use crate::windows_capture_target::TargetKind;
use tauri::Manager;
#[cfg(target_os = "windows")]
use windows::Win32::Foundation::HWND;
#[cfg(target_os = "windows")]
use windows::Win32::UI::WindowsAndMessaging::{GetWindowTextW, IsIconic};

const AUDIO_START_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const AUDIO_CLEANUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

// Combined window+display share cap (the receiver compositor's ceiling,
// matching macOS's 4-window limit).
pub(crate) const MAX_CONCURRENT_SHARES: usize = 4;
const SHARE_FIRST_FRAME_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const SHARE_LOSS_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
/// Per-interval push-cadence log from the share frame pump (the plan's
/// "measured rather than inferred" capture-to-push gate).
const SHARE_PUMP_HEALTH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
/// WGC only delivers frames when the captured content CHANGES (verified
/// live: a static window delivers its initial frame(s), then silence). The
/// pump re-pushes the last frame on this idle timer so receivers keep
/// receiving frames — macOS parity with `idle_static_refresh`
/// (`session/share.rs`), and REQUIRED for macOS receivers whose
/// no-frame watchdog would otherwise retire the window mid-share.
const SHARE_IDLE_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
/// Re-check the selected browser's current tab URL without republishing
/// unchanged metadata. A tab navigation is reflected on the next tick.
const SHARE_URL_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);
/// While a remote-control input landed within this window, the share frame
/// pump re-pushes at `RC_FPS_BOOST_REFRESH_INTERVAL` so the receiver stream
/// stays hot and responsive while the target reacts. WGC is change-driven
/// (it only delivers when content changes), so this is the FPS-side half of
/// the macOS quality-promotion-on-RC model — the pump can't invent content,
/// but it keeps the stream from idling and delivers each real WGC frame
/// promptly. The bitrate/quality half is a follow-up.
pub(crate) const RC_FPS_BOOST_WINDOW: std::time::Duration = std::time::Duration::from_millis(750);
const RC_FPS_BOOST_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(33);
/// Initial-burst window: for the first few seconds of a share the pump
/// re-pushes the last frame at the boost cadence even when the source is
/// static, so the encoder always has a fresh input frame and answers a new
/// subscriber's keyframe request within one frame-time (~33ms) instead of
/// waiting up to the idle-refresh interval (~2s). LiveKit's SFU requests a
/// keyframe when a subscriber attaches, and libwebrtc emits it on the NEXT
/// frame it receives — a 0.5fps idle pump can delay that by up to ~2s, a
/// large slice of the observed "first content after ~8s" window (the rest
/// being SFU publish propagation + subscription negotiation + delivery on
/// the path, which the app cannot shorten). Sized to cover the SFU
/// publish-propagation delay (~2.5s observed) plus the attach window; the
/// re-pushes are identical-content deltas, so the bandwidth cost is tiny.
const SHARE_INITIAL_BURST_WINDOW: std::time::Duration = std::time::Duration::from_secs(5);

use crate::transport::audio::audio_is_disabled;

/// Per-window share state shared between the frame pump and the session.
/// `published` swaps during a resize-republish; the pump reads it each
/// iteration, so the swap is visible without restarting the pump.
struct SharePumpShared {
    published: Mutex<Arc<crate::transport::publisher::PublishedTrack>>,
}

/// Window token -> instant until which the frame pump runs in remote-input
/// boost mode, raising its re-push cadence. Process-global on Windows;
/// written by the remote-control host path when an authorized input lands,
/// read by the share pump each iteration.
static REMOTE_INPUT_BOOST: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<u32, std::time::Instant>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// Raise the share frame pump's cadence for `window_id` for `duration` —
/// called on the host when a remote-control input is received.
pub(crate) fn boost_share_fps(window_id: u32, duration: std::time::Duration) {
    REMOTE_INPUT_BOOST
        .lock_unpoisoned()
        .insert(window_id, std::time::Instant::now() + duration);
}

/// Whether `window_id`'s share is currently inside the remote-input boost
/// window (prunes expired entries while it's here).
pub(crate) fn rc_boost_active(window_id: u32) -> bool {
    let now = std::time::Instant::now();
    let mut boost = REMOTE_INPUT_BOOST.lock_unpoisoned();
    let active = boost.get(&window_id).is_some_and(|until| *until > now);
    boost.retain(|_, until| *until > now);
    active
}

#[cfg(test)]
mod boost_tests {
    use super::*;

    #[test]
    fn rc_boost_active_respects_window_and_expiry() {
        REMOTE_INPUT_BOOST.lock_unpoisoned().clear();
        boost_share_fps(42, std::time::Duration::from_millis(50));
        assert!(rc_boost_active(42), "boosted window is active");
        assert!(!rc_boost_active(43), "another window is not boosted");
        std::thread::sleep(std::time::Duration::from_millis(70));
        assert!(!rc_boost_active(42), "boost expires after the window");
    }
}

struct ActiveShare {
    capture: crate::windows_screen_capture::TargetCaptureSession,
    status: crate::windows_screen_capture::CaptureStatus,
    pump: tauri::async_runtime::JoinHandle<()>,
    url_refresh: Option<tauri::async_runtime::JoinHandle<()>>,
    token: u32,
    kind: SharedSourceKind,
    title: String,
    shared: Arc<SharePumpShared>,
    share_instance_id: String,
    /// Petal View is excluded from supported capture paths only while its WGC
    /// display-region share owns this lease. Idle selectors remain recordable.
    selector_capture_exclusion: Option<crate::region_window::SelectorCaptureExclusionLease>,
    /// Sharer-chosen host policy for this share (display shares are always
    /// full-control; window shares default to cursor-preserving).
    /// Gates which delivery routes the host uses; never a controller change.
    control_mode: crate::remote_control_core::RemoteControlMode,
    /// Whether remote peers may control this specific share. The remote-control
    /// receiver re-reads this host-side value for every request and input.
    allow_remote_control: AtomicBool,
    /// On-screen frame (physical px on Windows) of the shared window, seeded
    /// at share start and kept fresh by the telepointer sender loop (~9Hz,
    /// `GetWindowRect` per share). Feeds `shared_windows_snapshot` for cursor
    /// hit-testing/normalization. Physical px is fine here: telepointer
    /// normalization is a ratio (scale-invariant), and remote control (the
    /// logical-point consumer on macOS) does not exist on Windows yet.
    frame: Mutex<crate::platform::cg::WindowFrame>,
    /// Logical companion source for this share, if its target exposes one.
    audio_source: Option<AudioSourceKey>,
    /// Unique registry owner. It must not be the reusable window token: a
    /// detached stop from an older share can otherwise release a replacement.
    audio_share_id: u64,
}

#[derive(Clone)]
pub(crate) struct WindowsControlTarget {
    pub(crate) window_id: u32,
    pub(crate) share_instance_id: String,
    pub(crate) kind: TargetKind,
    pub(crate) target: crate::windows_capture_target::WindowsCaptureTarget,
    pub(crate) content_frame: crate::platform::cg::WindowFrame,
    pub(crate) generation: RoomGeneration,
}

#[derive(Default)]
struct MediaResources {
    camera: Option<ActiveCamera>,
    microphone: Option<Arc<crate::transport::audio::MicTrack>>,
    playout: Option<crate::transport::audio::SpeakerPlayout>,
    screen_audio_sources: AudioSourceRegistry,
    screen_audio: BTreeMap<AudioSourceKey, Arc<ScreenAudioTrack>>,
    shares: Vec<ActiveShare>,
}

#[derive(Clone)]
struct MediaWatcherCancellation(tokio::sync::watch::Sender<bool>);

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum SharedWindowScreenStatus {
    NotShared,
    OnScreen(crate::platform::cg::WindowFrame),
    OffScreen,
    Closed,
}

impl MediaWatcherCancellation {
    fn cancel(&self) {
        let _ = self.0.send(true);
    }
}

fn media_watcher_cancellation() -> (MediaWatcherCancellation, tokio::sync::watch::Receiver<bool>) {
    let (sender, receiver) = tokio::sync::watch::channel(false);
    (MediaWatcherCancellation(sender), receiver)
}

struct WindowsMediaSession {
    room_record: crate::rooms::RoomRecord,
    identity: String,
    room_connection: Arc<RoomConnection>,
    presence: Arc<crate::presence::PresenceState>,
    media: MediaResources,
    watcher_cancellation: Option<MediaWatcherCancellation>,
}

/// Windows owns one native room connection and every media resource attached
/// to it. The transition lock makes join/switch/leave linearizable even when
/// two frontend requests arrive close together.
pub struct SessionState {
    joined: Mutex<Option<WindowsMediaSession>>,
    transition_lock: tokio::sync::Mutex<()>,
    mic_control_lock: tokio::sync::Mutex<()>,
    camera_control_lock: tokio::sync::Mutex<()>,
    share_control_lock: tokio::sync::Mutex<()>,
    audio_device_lock: tokio::sync::Mutex<()>,
    /// Serializes screen-audio source handoffs and publication starts; the
    /// joined-state mutex is never held across native/LiveKit awaits.
    screen_audio_control_lock: tokio::sync::Mutex<()>,
    /// `RemoteControlPolicy::as_u8`; live gate + the default "on" restores
    /// to. Same shape as the macOS session (session/mod.rs).
    remote_control_policy: AtomicU8,
    remote_control_default_policy: AtomicU8,
    desired_mic_muted: AtomicBool,
    desired_camera_on: AtomicBool,
    room_generation: Arc<AtomicU64>,
    /// Ensures at most one camera self-heal loop runs at a time (see
    /// `camera_session::ensure_camera_published`).
    camera_heal_active: AtomicBool,
    next_audio_share_id: AtomicU64,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            joined: Mutex::new(None),
            transition_lock: tokio::sync::Mutex::new(()),
            mic_control_lock: tokio::sync::Mutex::new(()),
            camera_control_lock: tokio::sync::Mutex::new(()),
            share_control_lock: tokio::sync::Mutex::new(()),
            audio_device_lock: tokio::sync::Mutex::new(()),
            screen_audio_control_lock: tokio::sync::Mutex::new(()),
            remote_control_policy: AtomicU8::new(RemoteControlPolicy::default().as_u8()),
            remote_control_default_policy: AtomicU8::new(RemoteControlPolicy::default().as_u8()),
            desired_mic_muted: AtomicBool::new(true),
            desired_camera_on: AtomicBool::new(false),
            room_generation: Arc::new(AtomicU64::new(0)),
            camera_heal_active: AtomicBool::new(false),
            next_audio_share_id: AtomicU64::new(0),
        }
    }
}

impl SessionState {
    pub(crate) fn begin_room_generation(&self) -> RoomGeneration {
        let value = self.room_generation.fetch_add(1, Ordering::SeqCst) + 1;
        RoomGeneration::new(self.room_generation.clone(), value)
    }

    pub(crate) fn current_room_generation(&self) -> RoomGeneration {
        RoomGeneration::new(
            self.room_generation.clone(),
            self.room_generation.load(Ordering::SeqCst),
        )
    }

    fn next_audio_share_id(&self) -> u64 {
        self.next_audio_share_id.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub(crate) fn invalidate_room_generation(&self) {
        self.room_generation.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn is_in_room(&self) -> bool {
        self.joined.lock_unpoisoned().is_some()
    }

    pub(crate) async fn lock_screen_audio_control(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.screen_audio_control_lock.lock().await
    }

    pub(crate) fn screen_audio_repair_snapshot(
        &self,
        reconnect_guard: &RoomGeneration,
    ) -> Option<(
        Arc<RoomConnection>,
        Vec<(AudioSourceKey, Option<Arc<ScreenAudioTrack>>)>,
    )> {
        let joined = self.joined.lock_unpoisoned();
        let session = joined.as_ref()?;
        if !reconnect_guard.is_current() {
            return None;
        }
        let sources = session
            .media
            .screen_audio_sources
            .active_source_keys()
            .into_iter()
            .map(|source| (source, session.media.screen_audio.get(&source).cloned()))
            .collect();
        Some((session.room_connection.clone(), sources))
    }

    pub(crate) fn screen_audio_is_current(
        &self,
        source: AudioSourceKey,
        expected: &Arc<ScreenAudioTrack>,
    ) -> bool {
        self.joined
            .lock_unpoisoned()
            .as_ref()
            .and_then(|session| session.media.screen_audio.get(&source))
            .is_some_and(|current| Arc::ptr_eq(current, expected))
    }

    pub(crate) fn screen_audio_source_is_active(&self, source: AudioSourceKey) -> bool {
        self.joined
            .lock_unpoisoned()
            .as_ref()
            .is_some_and(|session| session.media.screen_audio_sources.is_active(source))
    }

    pub(crate) fn screen_audio_handle_present(&self, source: AudioSourceKey) -> bool {
        self.joined
            .lock_unpoisoned()
            .as_ref()
            .is_some_and(|session| session.media.screen_audio.contains_key(&source))
    }

    fn current_room_name(&self) -> Option<String> {
        self.joined
            .lock_unpoisoned()
            .as_ref()
            .map(|joined| joined.room_record.name.clone())
    }

    fn presence_snapshot(&self) -> Vec<crate::presence::PresentParticipant> {
        self.joined
            .lock_unpoisoned()
            .as_ref()
            .map(|joined| joined.presence.snapshot())
            .unwrap_or_default()
    }

    /// Same contract as the macOS session's `joined_room_for_rename`.
    fn joined_room_for_rename(
        &self,
    ) -> Option<(
        Arc<livekit::Room>,
        Arc<crate::presence::PresenceState>,
        String,
    )> {
        self.joined.lock_unpoisoned().as_ref().map(|joined| {
            (
                joined.room_connection.room(),
                joined.presence.clone(),
                joined.room_record.name.clone(),
            )
        })
    }

    pub(crate) fn remote_control_allowed(&self) -> bool {
        self.remote_control_policy().allows_requests()
    }

    pub(crate) fn set_remote_control_allowed(&self, allowed: bool) {
        let next = RemoteControlPolicy::from_allowed(allowed, self.remote_control_default_policy());
        self.remote_control_policy
            .store(next.as_u8(), Ordering::Relaxed);
    }

    pub(crate) fn remote_control_policy(&self) -> RemoteControlPolicy {
        RemoteControlPolicy::from_u8(self.remote_control_policy.load(Ordering::Relaxed))
    }

    pub(crate) fn remote_control_default_policy(&self) -> RemoteControlPolicy {
        RemoteControlPolicy::from_u8(self.remote_control_default_policy.load(Ordering::Relaxed))
    }

    /// Seed BOTH the live meeting policy and the default it restores to.
    /// Used on join (from Settings) and by the autotest rig.
    pub(crate) fn seed_remote_control_policy(&self, policy: RemoteControlPolicy) {
        self.remote_control_default_policy
            .store(policy.as_u8(), Ordering::Relaxed);
        self.remote_control_policy
            .store(policy.as_u8(), Ordering::Relaxed);
    }

    /// Settings change (possibly mid-meeting): always update the default,
    /// but if the per-meeting pill has the live gate OFF, leave it off --
    /// the meeting route renders that pill from a one-time read and must
    /// not silently disagree with reality (adversarial review P3). Turning
    /// the default Off turns the live gate off too.
    pub(crate) fn set_remote_control_policy(&self, policy: RemoteControlPolicy) {
        self.remote_control_default_policy
            .store(policy.as_u8(), Ordering::Relaxed);
        if self.remote_control_policy().allows_requests() || !policy.allows_requests() {
            self.remote_control_policy
                .store(policy.as_u8(), Ordering::Relaxed);
        }
    }

    /// Per-share host control mode (display-like shares, including Petal View
    /// regions, are always full-control; ordinary window shares default to
    /// cursor-preserving). Used by the remote-control admission gate and the
    /// escalation flip.
    pub(crate) fn control_mode_for(
        &self,
        window_id: u32,
    ) -> crate::remote_control_core::RemoteControlMode {
        let joined = self.joined.lock_unpoisoned();
        joined
            .as_ref()
            .and_then(|s| s.media.shares.iter().find(|share| share.token == window_id))
            .map(|share| share.control_mode)
            .or_else(|| {
                crate::region_window::resolve(window_id)
                    .map(|_| crate::remote_control_core::RemoteControlMode::FullControl)
            })
            .unwrap_or(crate::remote_control_core::RemoteControlMode::CursorPreserving)
    }

    pub(crate) fn control_target_snapshot(
        &self,
        window_id: u32,
        share_instance_id: &str,
    ) -> Option<WindowsControlTarget> {
        let joined = self.joined.lock_unpoisoned();
        let share = joined.as_ref()?.media.shares.iter().find(|share| {
            share.token == window_id && share.share_instance_id == share_instance_id
        })?;
        let target = crate::windows_capture_target::resolve(share.token).ok()?;
        // A Petal View is captured as a display ROI, but its opaque target
        // token still resolves to the selector HWND (TargetKind::Window).
        // Validate the native target against that fact; expose the share to
        // remote control as display-like below.
        let expected_target_kind = match share.kind {
            SharedSourceKind::Window | SharedSourceKind::DisplayRegion => TargetKind::Window,
            SharedSourceKind::Display => TargetKind::Display,
        };
        if target.kind() != expected_target_kind {
            return None;
        }
        let client = *share.frame.lock_unpoisoned();
        let display_like = matches!(
            share.kind,
            SharedSourceKind::Display | SharedSourceKind::DisplayRegion
        );
        let outer = if share.kind == SharedSourceKind::DisplayRegion {
            region_content_frame(window_id).unwrap_or_else(|| current_target_frame(target, client))
        } else {
            current_target_frame(target, client)
        };
        Some(WindowsControlTarget {
            window_id,
            share_instance_id: share.share_instance_id.clone(),
            kind: if display_like {
                TargetKind::Display
            } else {
                target.kind()
            },
            target,
            content_frame: if display_like {
                outer
            } else {
                telepointer_content_frame(client, outer, share.capture.captured_size())
            },
            generation: self.current_room_generation(),
        })
    }

    pub(crate) fn active_share_frame(
        &self,
        window_id: u32,
    ) -> Option<crate::platform::cg::WindowFrame> {
        let share_instance_id = self
            .joined
            .lock_unpoisoned()
            .as_ref()?
            .media
            .shares
            .iter()
            .find(|share| share.token == window_id)?
            .share_instance_id
            .clone();
        self.control_target_snapshot(window_id, &share_instance_id)
            .map(|target| target.content_frame)
    }

    /// Human title of a LOCAL active share (consent prompt copy). None when
    /// that token is not being shared.
    pub(crate) fn active_share_source_title(&self, window_id: u32) -> Option<String> {
        self.joined
            .lock_unpoisoned()
            .as_ref()?
            .media
            .shares
            .iter()
            .find(|share| share.token == window_id)
            .map(|share| share.title.clone())
            .filter(|title| !title.trim().is_empty())
    }

    /// Whether this live share permits remote control. Unknown shares fail
    /// closed because there is nothing valid to authorize.
    pub(crate) fn share_allows_remote_control(&self, window_id: u32) -> bool {
        self.joined
            .lock_unpoisoned()
            .as_ref()
            .and_then(|session| {
                session
                    .media
                    .shares
                    .iter()
                    .find(|share| share.token == window_id)
            })
            .is_some_and(|share| share.allow_remote_control.load(Ordering::Relaxed))
    }

    /// Set the per-share lock, returning the previous value when the share exists.
    pub(crate) fn set_share_allows_remote_control(
        &self,
        window_id: u32,
        allowed: bool,
    ) -> Option<bool> {
        let joined = self.joined.lock_unpoisoned();
        let share = joined
            .as_ref()?
            .media
            .shares
            .iter()
            .find(|share| share.token == window_id)?;
        Some(share.allow_remote_control.swap(allowed, Ordering::Relaxed))
    }

    /// The live room connection used to publish per-share metadata.
    pub(crate) fn room_connection(&self) -> Option<Arc<RoomConnection>> {
        self.joined
            .lock_unpoisoned()
            .as_ref()
            .map(|session| session.room_connection.clone())
    }

    pub(crate) fn active_share_is_display(&self, window_id: u32) -> bool {
        self.joined
            .lock_unpoisoned()
            .as_ref()
            .and_then(|session| {
                session
                    .media
                    .shares
                    .iter()
                    .find(|share| share.token == window_id)
            })
            .is_some_and(|share| {
                matches!(
                    share.kind,
                    SharedSourceKind::Display | SharedSourceKind::DisplayRegion
                )
            })
    }

    pub(crate) fn active_share_pid(&self, window_id: u32) -> Option<i32> {
        let share_instance_id = self
            .joined
            .lock_unpoisoned()
            .as_ref()?
            .media
            .shares
            .iter()
            .find(|share| share.token == window_id)?
            .share_instance_id
            .clone();
        let pid = self
            .control_target_snapshot(window_id, &share_instance_id)?
            .target
            .owner_process_id();
        (pid > 0).then_some(pid as i32)
    }

    /// Return the capable envelope for an active application-window share.
    /// Native clipboard Paste uses this to reuse the same target identity that
    /// the Windows controller grant was authorized against.
    pub(crate) fn active_window_control_envelope(
        &self,
        window_id: u32,
    ) -> Option<(crate::remote_control_core::RemoteControlTargetKind, String)> {
        let joined = self.joined.lock_unpoisoned();
        let share = joined
            .as_ref()?
            .media
            .shares
            .iter()
            .find(|share| share.token == window_id)?;
        if share.kind != SharedSourceKind::Window {
            return None;
        }
        let share_instance_id = share.share_instance_id.clone();
        drop(joined);
        self.control_target_snapshot(window_id, &share_instance_id)?;
        Some((
            crate::remote_control_core::RemoteControlTargetKind::Window,
            share_instance_id,
        ))
    }

    pub(crate) fn shared_window_screen_status(&self, window_id: u32) -> SharedWindowScreenStatus {
        let share_instance_id = {
            let joined = self.joined.lock_unpoisoned();
            let Some(share) = joined.as_ref().and_then(|joined| {
                joined
                    .media
                    .shares
                    .iter()
                    .find(|share| share.token == window_id)
            }) else {
                return SharedWindowScreenStatus::NotShared;
            };
            share.share_instance_id.clone()
        };
        let Some(target) = self.control_target_snapshot(window_id, &share_instance_id) else {
            return SharedWindowScreenStatus::Closed;
        };
        if target.kind == TargetKind::Window
            && !crate::windows_remote_control::window_is_on_screen(target.target.raw_handle())
        {
            return SharedWindowScreenStatus::OffScreen;
        }
        SharedWindowScreenStatus::OnScreen(target.content_frame)
    }

    pub(crate) async fn lock_mic_control(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.mic_control_lock.lock().await
    }

    pub(crate) async fn lock_camera_control(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.camera_control_lock.lock().await
    }

    pub(crate) async fn lock_share_control(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.share_control_lock.lock().await
    }

    pub(crate) fn shared_window_ids(&self) -> Vec<u32> {
        self.joined
            .lock_unpoisoned()
            .as_ref()
            .map(|session| {
                session
                    .media
                    .shares
                    .iter()
                    .map(|share| share.token)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// macOS `session::SessionState::is_share_active` parity (the AI chat
    /// session engine and its commands call this per window): true while the
    /// given window id is a live local share. `shared_window_ids` is the
    /// single source of truth for the current share set, so this is a direct
    /// membership test on it.
    pub(crate) fn is_share_active(&self, window_id: u32) -> bool {
        self.shared_window_ids().contains(&window_id)
    }

    /// macOS `session::SessionState::current_room_record` parity (the AI chat
    /// commands read only `record.name`): the persisted record of the joined
    /// room, or `None` when not in a room.
    pub(crate) fn current_room_record(&self) -> Option<crate::rooms::RoomRecord> {
        self.joined
            .lock_unpoisoned()
            .as_ref()
            .map(|session| session.room_record.clone())
    }

    /// Snapshot for the telepointer sender: (publisher, local identity,
    /// (token, on-screen frame) per shared window). Mirrors the macOS
    /// `session::SessionState::shared_windows_snapshot` contract so the
    /// shared `telepointer` sender loop compiles unchanged on both platforms.
    /// The frame is the WGC-CAPTURED content region, not the raw client rect:
    /// WGC captures the client area minus the invisible DWM resize borders
    /// (the item size is ~7px narrower per side than `GetClientRect` on
    /// 96-DPI), so normalizing the cursor against the client rect would shift
    /// the remote tag right by the horizontal border (observed live). The
    /// capture is centered horizontally and top-aligned (the top client edge
    /// sits below the caption, which has no invisible border); maximized
    /// windows have no invisible borders, so the conversion no-ops there.
    /// All sizes are physical px — telepointer normalization is a
    /// scale-invariant ratio.
    pub(crate) fn shared_windows_snapshot(
        &self,
    ) -> (
        Option<Arc<RoomConnection>>,
        Option<String>,
        Vec<(u32, crate::platform::cg::WindowFrame)>,
    ) {
        let guard = self.joined.lock_unpoisoned();
        match guard.as_ref() {
            Some(session) => {
                let frames = session
                    .media
                    .shares
                    .iter()
                    .map(|share| {
                        let client = *share.frame.lock_unpoisoned();
                        let Some(target) = crate::windows_capture_target::resolve(share.token).ok()
                        else {
                            return (share.token, client);
                        };
                        let display_like = matches!(
                            share.kind,
                            SharedSourceKind::Display | SharedSourceKind::DisplayRegion
                        );
                        let outer = if share.kind == SharedSourceKind::DisplayRegion {
                            region_content_frame(share.token)
                                .unwrap_or_else(|| current_target_frame(target, client))
                        } else {
                            current_target_frame(target, client)
                        };
                        (
                            share.token,
                            if display_like {
                                outer
                            } else {
                                telepointer_content_frame(
                                    client,
                                    outer,
                                    share.capture.captured_size(),
                                )
                            },
                        )
                    })
                    .collect();
                (
                    Some(session.room_connection.clone()),
                    Some(session.identity.clone()),
                    frames,
                )
            }
            None => (None, None, Vec::new()),
        }
    }

    /// Frame refresh from the telepointer sender loop (~9Hz). Windows keeps no
    /// per-share visibility flags (WGC capture pausing is driven elsewhere and
    /// a minimized window naturally falls out of cursor hit-testing), so only
    /// the frames are applied — `visible_window_ids` is accepted for the
    /// shared-loop contract and ignored.
    pub(crate) fn update_share_frames_and_visibility(
        &self,
        fresh: &[(u32, crate::platform::cg::WindowFrame)],
        _visible_window_ids: &[u32],
    ) {
        let mut guard = self.joined.lock_unpoisoned();
        if let Some(session) = guard.as_mut() {
            for (window_id, frame) in fresh {
                if let Some(share) = session
                    .media
                    .shares
                    .iter_mut()
                    .find(|share| share.token == *window_id)
                {
                    *share.frame.lock_unpoisoned() = *frame;
                }
            }
        }
    }

    pub(crate) fn camera_publishing(&self) -> bool {
        self.joined
            .lock_unpoisoned()
            .as_ref()
            .map(|session| session.media.camera.is_some())
            .unwrap_or(false)
    }

    pub(crate) fn camera_intent(&self) -> bool {
        self.desired_camera_on.load(Ordering::SeqCst)
    }

    pub(crate) fn set_camera_intent(&self, enabled: bool) {
        self.desired_camera_on.store(enabled, Ordering::SeqCst);
    }

    /// Snapshot of the room data channel + local identity, read under one
    /// lock acquisition (same shape as macOS's `control_channel_snapshot`).
    /// Used by `camera_session` to publish the camera track to the current
    /// room connection.
    pub(crate) fn control_channel_snapshot(&self) -> Option<(Arc<RoomConnection>, String)> {
        self.joined
            .lock_unpoisoned()
            .as_ref()
            .map(|session| (session.room_connection.clone(), session.identity.clone()))
    }

    /// Try to become the single active camera self-heal loop. Returns false
    /// if one is already running (it will converge on the current intent).
    pub(crate) fn try_begin_camera_heal(&self) -> bool {
        self.camera_heal_active
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    pub(crate) fn end_camera_heal(&self) {
        self.camera_heal_active.store(false, Ordering::SeqCst);
    }

    /// Take the current camera out of the joined media session, if any.
    /// `None` when not joined (or nothing is publishing).
    pub(crate) fn take_active_camera(&self) -> Option<ActiveCamera> {
        self.joined
            .lock_unpoisoned()
            .as_mut()
            .and_then(|session| session.media.camera.take())
    }

    /// Store the camera into the joined media session, atomically re-checking
    /// that the room is still joined (leave_room can race a mid-start camera).
    /// Takes the value out of `camera` ONLY on success; on rejection the
    /// caller keeps ownership and tears the capture down itself.
    pub(crate) fn put_active_camera(&self, camera: &mut Option<ActiveCamera>) -> bool {
        let mut joined = self.joined.lock_unpoisoned();
        match joined.as_mut() {
            Some(session) => {
                session.media.camera = camera.take();
                true
            }
            None => camera.is_none(),
        }
    }

    /// Take the camera ONLY when it is the capture `status` belongs to — a
    /// stop+restart since the caller's last poll must leave the newer camera
    /// alone. Used by the loss monitor.
    pub(crate) fn take_active_camera_matching(
        &self,
        status: &crate::transport::camera::CameraStatus,
    ) -> Option<ActiveCamera> {
        let mut joined = self.joined.lock_unpoisoned();
        let session = joined.as_mut()?;
        if session
            .media
            .camera
            .as_ref()
            .map(|camera| camera.status.same_capture(status))
            .unwrap_or(false)
        {
            session.media.camera.take()
        } else {
            None
        }
    }

    pub(crate) fn mic_muted(&self) -> bool {
        self.joined
            .lock_unpoisoned()
            .as_ref()
            .and_then(|session| session.media.microphone.clone())
            .map(|mic| mic.is_muted())
            .unwrap_or_else(|| self.desired_mic_muted.load(Ordering::SeqCst))
    }

    pub(crate) fn set_mic_muted(&self, muted: bool) -> Result<bool, String> {
        let joined = self.joined.lock_unpoisoned();
        let microphone = joined
            .as_ref()
            .and_then(|session| session.media.microphone.as_ref())
            .ok_or_else(|| "microphone unavailable".to_string())?;

        microphone.set_muted(muted);
        let reached = microphone.is_muted();
        if reached != muted {
            return Err("microphone did not reach requested mute state".to_string());
        }
        self.desired_mic_muted.store(reached, Ordering::SeqCst);
        Ok(reached)
    }

    /// Windows AI-chat parity: temporarily mute the live microphone without
    /// changing the user's persistent mute intent.
    pub(crate) fn set_ai_chat_ducking(&self, duck: bool) {
        let muted = self.desired_mic_muted.load(Ordering::SeqCst) || duck;
        if let Some(microphone) = self
            .joined
            .lock_unpoisoned()
            .as_ref()
            .and_then(|session| session.media.microphone.clone())
        {
            microphone.set_muted(muted);
        }
    }

    pub(crate) async fn set_audio_devices(
        &self,
        recording_id: Option<String>,
        playout_id: Option<String>,
        preferences: &crate::transport::audio::AudioDevicePreferences,
    ) -> crate::transport::audio::AppliedAudioDevices {
        let _device_transaction = self.audio_device_lock.lock().await;
        if let Some(id) = &recording_id {
            preferences.set_recording_device(id.clone());
        }
        if let Some(id) = &playout_id {
            preferences.set_playout_device(id.clone());
        }
        let joined = self.joined.lock_unpoisoned();
        let Some(session) = joined.as_ref() else {
            return crate::transport::audio::AppliedAudioDevices::default();
        };
        let mut result = crate::transport::audio::AppliedAudioDevices {
            in_room: true,
            ..Default::default()
        };

        if let Some(id) = recording_id {
            match session.media.microphone.as_ref() {
                Some(microphone) => {
                    let switched = if id.is_empty() {
                        microphone.use_default_recording_device()
                    } else {
                        microphone.set_recording_device(&id)
                    };
                    match switched {
                        Ok(_) => result.mic_applied = true,
                        Err(error) => result.mic_error = Some(error),
                    }
                }
                None => result.mic_error = Some("no live microphone track".to_string()),
            }
        }

        if let Some(id) = playout_id {
            match session.media.playout.as_ref() {
                Some(playout) => {
                    let switched = if id.is_empty() {
                        playout.use_default_playout_device()
                    } else {
                        playout.set_playout_device(&id)
                    };
                    match switched {
                        Ok(_) => result.speaker_applied = true,
                        Err(error) => result.speaker_error = Some(error),
                    }
                }
                None => result.speaker_error = Some("no live speaker playout".to_string()),
            }
        }

        result
    }

    fn refresh_audio_devices(
        &self,
    ) -> (
        Option<crate::transport::audio::RecordingDeviceRefresh>,
        Option<crate::transport::audio::PlayoutDeviceRefresh>,
    ) {
        let joined = self.joined.lock_unpoisoned();
        let recording = joined
            .as_ref()
            .and_then(|session| session.media.microphone.as_ref())
            .map(|microphone| microphone.refresh_default_recording_device());
        let playout = joined
            .as_ref()
            .and_then(|session| session.media.playout.as_ref())
            .map(|playout| playout.refresh_default_playout_device());
        (recording, playout)
    }
}

fn is_same_room(current: &crate::rooms::RoomRecord, candidate: &crate::rooms::RoomRecord) -> bool {
    current.id == candidate.id
}

#[derive(Debug, thiserror::Error, Clone, serde::Serialize)]
#[serde(tag = "kind", content = "message", rename_all = "camelCase")]
pub enum SessionError {
    #[error("missing LiveKit configuration: {0}")]
    Config(String),
    #[error("failed to connect to LiveKit room: {0}")]
    RoomConnect(String),
}

impl From<crate::meeting_core::RoomJoinError> for SessionError {
    fn from(error: crate::meeting_core::RoomJoinError) -> Self {
        match error {
            crate::meeting_core::RoomJoinError::Config(message) => Self::Config(message),
            crate::meeting_core::RoomJoinError::RoomConnect(message) => Self::RoomConnect(message),
        }
    }
}

/// Share start/stop failures. The serde `kind` strings deliberately reuse
/// the macOS `ShareSessionError` kind names the frontend's `shareErrorDisplay`
/// already maps to readable toasts — no new kind strings.
#[derive(Debug, Clone, thiserror::Error, serde::Serialize)]
#[serde(tag = "kind", content = "message", rename_all = "camelCase")]
pub enum ShareError {
    #[error("not in a room")]
    NotInRoom,
    #[error("shared window {0} not found")]
    WindowNotFound(String),
    #[error("capture failed: {0}")]
    Capture(String),
    #[error("too many simultaneous shares (max 4)")]
    TooManyShares,
    #[error("unknown share error: {0}")]
    Unknown(String),
}

// ===========================================================================
// Window + display share publication (WGC capture, kind-aware)
// ===========================================================================

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ShareStateChangedEvent {
    window_id: u32,
    shared: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ShareErrorPayload {
    window_id: u32,
    was_starting: bool,
    error: ShareError,
}

pub(crate) fn emit_share_state_changed(app: &tauri::AppHandle, window_id: u32, shared: bool) {
    if let Err(error) = tauri::Emitter::emit(
        app,
        "share-state-changed",
        ShareStateChangedEvent { window_id, shared },
    ) {
        log::warn!("windows session: failed to emit share-state-changed: {error}");
    }
    crate::region_window::emit_region_share_state(app, window_id, shared);
}

pub(crate) fn emit_share_error(
    app: &tauri::AppHandle,
    window_id: u32,
    was_starting: bool,
    error: ShareError,
) {
    if let Err(emit_error) = tauri::Emitter::emit(
        app,
        "share-error",
        ShareErrorPayload {
            window_id,
            was_starting,
            error,
        },
    ) {
        log::warn!("windows session: failed to emit share-error: {emit_error}");
    }
}

/// Share source title for the wire metadata: "Screen N" for displays (the
/// stable registry ordinal), the window's current text for windows.
fn share_title_for_target(target: crate::windows_capture_target::WindowsCaptureTarget) -> String {
    match target.kind() {
        TargetKind::Display => {
            format!("Screen {}", target.display_ordinal().unwrap_or_default())
        }
        TargetKind::Window => {
            let hwnd = HWND(target.raw_handle() as *mut core::ffi::c_void);
            let mut buf = [0u16; 512];
            let len = unsafe { GetWindowTextW(hwnd, &mut buf) };
            if len <= 0 {
                "Shared window".to_string()
            } else {
                String::from_utf16_lossy(&buf[..(len as usize).min(buf.len())])
            }
        }
    }
}

fn new_share_instance_id(token: u32) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{:x}-{token:x}-{sequence:x}", crate::time_util::now_ms())
}

/// Minimized-window gate (window shares only; displays never minimize).
/// While minimized the share holds its last frame and resumes on restore.
fn share_target_is_minimized(token: u32) -> bool {
    let Ok(target) = crate::windows_capture_target::resolve(token) else {
        return false;
    };
    if target.kind() != TargetKind::Window {
        return false;
    }
    let hwnd = HWND(target.raw_handle() as *mut core::ffi::c_void);
    unsafe { IsIconic(hwnd) }.as_bool()
}

async fn drop_share_capture(capture: crate::windows_screen_capture::TargetCaptureSession) {
    if tokio::task::spawn_blocking(move || drop(capture))
        .await
        .is_err()
    {
        log::warn!("windows session: share capture cleanup task failed");
    }
}

const SCREEN_AUDIO_FAILURE_RESTART_ATTEMPTS: usize = 3;
const SCREEN_AUDIO_FAILURE_RESTART_DELAY: std::time::Duration =
    std::time::Duration::from_millis(250);
const SCREEN_AUDIO_FAILURE_POLL: std::time::Duration = std::time::Duration::from_millis(500);

async fn publish_screen_audio_if_needed(
    app: &tauri::AppHandle,
    state: &SessionState,
    room_connection: &Arc<RoomConnection>,
    generation: &RoomGeneration,
    source: AudioSourceKey,
) -> bool {
    let (should_start, already_present) = {
        let joined = state.joined.lock_unpoisoned();
        let current_room = generation.is_current()
            && joined
                .as_ref()
                .is_some_and(|session| Arc::ptr_eq(&session.room_connection, room_connection));
        (
            current_room
                && joined.as_ref().is_some_and(|session| {
                    session.media.screen_audio_sources.is_active(source)
                        && !session.media.screen_audio.contains_key(&source)
                }),
            current_room
                && joined
                    .as_ref()
                    .is_some_and(|session| session.media.screen_audio.contains_key(&source)),
        )
    };
    if !should_start {
        return already_present;
    }

    let label = source.label();
    let track = match ScreenAudioTrack::publish(room_connection.room(), source, move |error| {
        log::warn!("screen-audio source '{label}' failed: {error}")
    })
    .await
    {
        Ok(track) => Arc::new(track),
        Err(error) => {
            log::warn!(
                "windows session: screen-audio source '{}' unavailable; continuing video-only: {error}",
                source.label()
            );
            return false;
        }
    };

    let committed = {
        let mut joined = state.joined.lock_unpoisoned();
        let current = generation.is_current()
            && joined.as_mut().is_some_and(|session| {
                Arc::ptr_eq(&session.room_connection, room_connection)
                    && session.media.screen_audio_sources.is_active(source)
                    && !session.media.screen_audio.contains_key(&source)
            });
        if current {
            joined
                .as_mut()
                .expect("current Windows media session")
                .media
                .screen_audio
                .insert(source, track.clone());
            true
        } else {
            false
        }
    };
    if committed {
        spawn_screen_audio_failure_monitor(app, generation.clone(), source, track);
        true
    } else {
        track.stop().await;
        false
    }
}

async fn publish_screen_audio_with_retries(
    app: &tauri::AppHandle,
    state: &SessionState,
    room_connection: &Arc<RoomConnection>,
    generation: &RoomGeneration,
    source: AudioSourceKey,
) -> bool {
    if crate::transport::audio::audio_disabled_by_env() {
        return false;
    }
    for attempt in 1..=SCREEN_AUDIO_FAILURE_RESTART_ATTEMPTS {
        if attempt > 1 {
            tokio::time::sleep(SCREEN_AUDIO_FAILURE_RESTART_DELAY * (attempt as u32 - 1)).await;
        }
        let serial = state.lock_screen_audio_control().await;
        if !generation.is_current()
            || !state.is_in_room()
            || !state.screen_audio_source_is_active(source)
        {
            drop(serial);
            return false;
        }
        if publish_screen_audio_if_needed(app, state, room_connection, generation, source).await {
            drop(serial);
            if attempt > 1 {
                log::info!(
                    "windows session: screen-audio source '{}' started on retry {attempt}/{SCREEN_AUDIO_FAILURE_RESTART_ATTEMPTS}",
                    source.label()
                );
            }
            return true;
        }
        drop(serial);
    }
    log::warn!(
        "windows session: screen-audio source '{}' unavailable after {SCREEN_AUDIO_FAILURE_RESTART_ATTEMPTS} start attempts; continuing video-only",
        source.label()
    );
    false
}

async fn apply_screen_audio_transitions(
    state: &SessionState,
    transitions: Vec<AudioSourceTransition>,
) -> Vec<AudioSourceKey> {
    let mut starts = Vec::new();
    for transition in transitions {
        match transition {
            AudioSourceTransition::Stop(source) => {
                let track = state
                    .joined
                    .lock_unpoisoned()
                    .as_mut()
                    .and_then(|session| session.media.screen_audio.remove(&source));
                if let Some(track) = track {
                    track.stop().await;
                }
            }
            AudioSourceTransition::Start(source) => starts.push(source),
        }
    }
    starts
}

async fn acquire_screen_audio_source(
    app: &tauri::AppHandle,
    state: &SessionState,
    room_connection: &Arc<RoomConnection>,
    generation: &RoomGeneration,
    share_id: u64,
    source: AudioSourceKey,
) {
    if crate::transport::audio::audio_disabled_by_env() {
        log::debug!(
            "windows session: PETAL_DISABLE_AUDIO set -- skipping screen-audio source '{}'",
            source.label()
        );
        return;
    }
    let serial = state.lock_screen_audio_control().await;
    let transitions = {
        let mut joined = state.joined.lock_unpoisoned();
        let Some(session) = joined.as_mut() else {
            drop(serial);
            return;
        };
        if !generation.is_current() || !Arc::ptr_eq(&session.room_connection, room_connection) {
            drop(serial);
            return;
        }
        session.media.screen_audio_sources.acquire(share_id, source)
    };
    let starts = apply_screen_audio_transitions(state, transitions).await;
    let source_was_started = starts.contains(&source);
    drop(serial);

    // Start only after all old owners have been stopped. Retries acquire the
    // control lock per attempt so a stop can cancel the backoff promptly.
    for start in starts {
        let _ =
            publish_screen_audio_with_retries(app, state, room_connection, generation, start).await;
    }
    // A previous capture/publication failure can leave the desired source
    // registered but without a handle. Retry that source even when this
    // acquisition did not change the active set. A source just started above
    // already had its bounded retry budget consumed; do not run a second
    // budget in the same acquisition.
    if !source_was_started
        && generation.is_current()
        && state.screen_audio_source_is_active(source)
        && !state.screen_audio_handle_present(source)
    {
        let _ = publish_screen_audio_with_retries(app, state, room_connection, generation, source)
            .await;
    }
}

async fn ensure_screen_audio_source(
    app: &tauri::AppHandle,
    state: &SessionState,
    room_connection: &Arc<RoomConnection>,
    generation: &RoomGeneration,
    source: AudioSourceKey,
) {
    let _ =
        publish_screen_audio_with_retries(app, state, room_connection, generation, source).await;
}

async fn release_screen_audio_source(
    app: &tauri::AppHandle,
    state: &SessionState,
    room_connection: Option<&Arc<RoomConnection>>,
    share_id: u64,
    source: AudioSourceKey,
) {
    let Some(room_connection) = room_connection else {
        return;
    };
    let generation = state.current_room_generation();
    let serial = state.lock_screen_audio_control().await;
    let transitions = {
        let mut joined = state.joined.lock_unpoisoned();
        let Some(session) = joined.as_mut() else {
            drop(serial);
            return;
        };
        if !Arc::ptr_eq(&session.room_connection, room_connection) {
            drop(serial);
            return;
        }
        if session
            .media
            .screen_audio_sources
            .source_for_share(share_id)
            != Some(source)
        {
            drop(serial);
            return;
        }
        session.media.screen_audio_sources.release(share_id)
    };
    let starts = apply_screen_audio_transitions(state, transitions).await;
    drop(serial);
    for start in starts {
        let _ = publish_screen_audio_with_retries(app, state, room_connection, &generation, start)
            .await;
    }
}

fn spawn_screen_audio_release(
    app: &tauri::AppHandle,
    room_connection: Option<Arc<RoomConnection>>,
    share_id: u64,
    source: AudioSourceKey,
) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let Some(state) = app.try_state::<SessionState>() else {
            return;
        };
        release_screen_audio_source(
            &app,
            state.inner(),
            room_connection.as_ref(),
            share_id,
            source,
        )
        .await;
    });
}

fn spawn_screen_audio_failure_monitor(
    app: &tauri::AppHandle,
    generation: RoomGeneration,
    source: AudioSourceKey,
    expected: Arc<ScreenAudioTrack>,
) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut interval = tokio::time::interval(SCREEN_AUDIO_FAILURE_POLL);
        loop {
            interval.tick().await;
            let Some(state) = app.try_state::<SessionState>() else {
                return;
            };
            if !generation.is_current()
                || !state.screen_audio_is_current(source, &expected)
                || expected.is_stopped()
            {
                return;
            }
            if !expected.capture_failed() {
                continue;
            }
            restart_failed_screen_audio_source(
                &app,
                state.inner(),
                generation.clone(),
                source,
                expected.clone(),
            )
            .await;
            return;
        }
    });
}

async fn restart_failed_screen_audio_source(
    app: &tauri::AppHandle,
    state: &SessionState,
    generation: RoomGeneration,
    source: AudioSourceKey,
    expected: Arc<ScreenAudioTrack>,
) {
    let serial = state.lock_screen_audio_control().await;
    let (old, room_connection) = {
        let mut joined = state.joined.lock_unpoisoned();
        let Some(session) = joined.as_mut() else {
            drop(serial);
            return;
        };
        let Some(current) = session.media.screen_audio.get(&source) else {
            drop(serial);
            return;
        };
        if !generation.is_current()
            || !Arc::ptr_eq(current, &expected)
            || !current.capture_failed()
            || !session.media.screen_audio_sources.is_active(source)
        {
            drop(serial);
            return;
        }
        let room_connection = session.room_connection.clone();
        let old = session
            .media
            .screen_audio
            .remove(&source)
            .expect("screen audio handle present");
        (old, room_connection)
    };
    // Do not let a handoff start while the failed publication is still being
    // unpublished and its native capture is stopping.
    old.stop().await;
    drop(serial);

    for attempt in 1..=SCREEN_AUDIO_FAILURE_RESTART_ATTEMPTS {
        if attempt > 1 {
            tokio::time::sleep(SCREEN_AUDIO_FAILURE_RESTART_DELAY * (attempt as u32 - 1)).await;
        }
        let serial = state.lock_screen_audio_control().await;
        if !generation.is_current() || !state.screen_audio_source_is_active(source) {
            drop(serial);
            return;
        }
        let label = source.label();
        match ScreenAudioTrack::publish(room_connection.room(), source, move |error| {
            log::warn!("screen-audio source '{label}' failed: {error}")
        })
        .await
        {
            Ok(track) => {
                let track = Arc::new(track);
                let committed = {
                    let mut joined = state.joined.lock_unpoisoned();
                    let current = generation.is_current()
                        && joined.as_mut().is_some_and(|session| {
                            Arc::ptr_eq(&session.room_connection, &room_connection)
                                && session.media.screen_audio_sources.is_active(source)
                                && !session.media.screen_audio.contains_key(&source)
                        });
                    if current {
                        joined
                            .as_mut()
                            .expect("current Windows media session")
                            .media
                            .screen_audio
                            .insert(source, track.clone());
                    }
                    current
                };
                if committed {
                    drop(serial);
                    log::info!(
                        "windows session: restarted failed screen-audio source '{}' on attempt {attempt}/{SCREEN_AUDIO_FAILURE_RESTART_ATTEMPTS}",
                        source.label()
                    );
                    spawn_screen_audio_failure_monitor(app, generation.clone(), source, track);
                } else {
                    // The failed commit still published a native/LiveKit
                    // resource; serialize its teardown with the next start.
                    track.stop().await;
                    drop(serial);
                }
                return;
            }
            Err(error) => {
                drop(serial);
                log::warn!(
                    "windows session: screen-audio source '{}' restart attempt {attempt}/{SCREEN_AUDIO_FAILURE_RESTART_ATTEMPTS} failed: {error}",
                    source.label()
                );
            }
        }
    }
    log::error!(
        "windows session: screen-audio source '{}' restart exhausted; continuing video-only",
        source.label()
    );
}

pub(crate) async fn start_share_token(
    app: tauri::AppHandle,
    state: &SessionState,
    token: u32,
    control_mode: crate::remote_control_core::RemoteControlMode,
    color: String,
) -> Result<bool, ShareError> {
    let is_region_share = crate::region_window::resolve(token).is_some();
    let (room_connection, identity, generation, target) = {
        let joined = state.joined.lock_unpoisoned();
        let Some(session) = joined.as_ref() else {
            return Err(ShareError::NotInRoom);
        };
        let target = crate::windows_capture_target::resolve(token)
            .map_err(|_| ShareError::WindowNotFound(token.to_string()))?;
        if !is_region_share
            && target.kind() == TargetKind::Window
            && !crate::share_target::classify_windows_window(
                windows::Win32::Foundation::HWND(target.raw_handle() as *mut core::ffi::c_void),
                std::process::id(),
            )
            .is_eligible()
        {
            return Err(ShareError::WindowNotFound(token.to_string()));
        }
        if session
            .media
            .shares
            .iter()
            .any(|share| share.token == token)
        {
            return Ok(true);
        }
        if session.media.shares.len() >= MAX_CONCURRENT_SHARES {
            return Err(ShareError::TooManyShares);
        }
        (
            session.room_connection.clone(),
            session.identity.clone(),
            state.current_room_generation(),
            target,
        )
    };
    let audio_share_id = state.next_audio_share_id();
    let control_mode = effective_control_mode(
        if is_region_share {
            TargetKind::Display
        } else {
            target.kind()
        },
        control_mode,
    );
    let share_instance_id = new_share_instance_id(token);
    // URL metadata is optional and must never delay the visible share border or
    // WGC startup. Supported browser targets are inspected by the cancellable
    // background refresh after the share is established.
    let source_url: Option<String> = None;
    // Window capture always keeps the native WGC gold border, including for
    // elevated targets. Petal must not request borderless consent merely to
    // replace a system indicator; custom compositor chrome remains available
    // for drawing/telepointer state but is never the capture-indicator authority.
    let borderless_access = if is_region_share || target.kind() == TargetKind::Display {
        crate::windows_screen_capture::request_borderless_access().await
    } else {
        crate::windows_screen_capture::BorderlessAccess::Denied
    };
    let kind = if is_region_share {
        SharedSourceKind::DisplayRegion
    } else {
        match target.kind() {
            TargetKind::Window => SharedSourceKind::Window,
            TargetKind::Display => SharedSourceKind::Display,
        }
    };
    // Petal View is already its own visible replacement. Ordinary window and
    // display shares use the existing sharer overlay; only its readiness may
    // authorize suppressing WGC's system indicator.
    let capture_source_kind = match kind {
        SharedSourceKind::Window => crate::windows_screen_capture::CaptureSourceKind::Window,
        SharedSourceKind::Display => crate::windows_screen_capture::CaptureSourceKind::Display,
        SharedSourceKind::DisplayRegion => {
            crate::windows_screen_capture::CaptureSourceKind::DisplayRegion
        }
    };
    // Petal View's selector is the visible replacement for the WGC border.
    // Acquire its display-affinity lease immediately before WGC starts; idle
    // selectors intentionally remain visible to supported screen recorders.
    let selector_capture_exclusion = if is_region_share {
        crate::region_window::acquire_selector_capture_exclusion(&app, token)
    } else {
        None
    };
    let selector_capture_excluded = if is_region_share {
        selector_capture_exclusion.is_some()
    } else {
        true
    };
    let requested_mode = crate::windows_screen_capture::capture_indicator_mode(
        borderless_access,
        is_region_share
            || borderless_access == crate::windows_screen_capture::BorderlessAccess::Allowed,
    );
    let overlay_readiness = crate::windows_share_overlay::create_share_overlay(
        &app,
        token,
        &identity,
        kind == SharedSourceKind::Display,
        requested_mode,
        is_region_share,
        &color,
    );
    let replacement_ready = if is_region_share {
        true
    } else {
        overlay_readiness
            .as_ref()
            .is_ok_and(|readiness| readiness.shown && readiness.custom_indicator_ready)
    };
    let capture_excluded = if is_region_share {
        selector_capture_excluded
    } else {
        overlay_readiness
            .as_ref()
            .is_ok_and(|readiness| readiness.capture_excluded)
    };
    let owner_verified =
        if capture_source_kind == crate::windows_screen_capture::CaptureSourceKind::Window {
            overlay_readiness
                .as_ref()
                .is_ok_and(|readiness| readiness.custom_indicator_ready)
        } else {
            false
        };
    let indicator_mode = crate::windows_screen_capture::capture_indicator_mode_for_source(
        borderless_access,
        capture_source_kind,
        replacement_ready,
        capture_excluded,
        owner_verified,
    );
    if let Err(error) = &overlay_readiness {
        log::warn!(
            "windows session: share token={token} overlay unavailable; using indicator mode={indicator_mode:?}: {error}"
        );
    } else {
        log::info!(
            "windows session: share token={token} indicator mode={indicator_mode:?} region={is_region_share}"
        );
    }

    let latest_frame = Arc::new(Mutex::new(None::<crate::windows_screen_capture::BgraFrame>));
    let frame_ready = Arc::new(tokio::sync::Notify::new());
    let first_frame_sent = Arc::new(AtomicBool::new(false));
    let (first_frame_tx, first_frame_rx) = std::sync::mpsc::sync_channel(1);

    let callback_latest_frame = latest_frame.clone();
    let callback_frame_ready = frame_ready.clone();
    let callback_first_frame_sent = first_frame_sent.clone();
    let start_task = tokio::task::spawn_blocking(move || {
        crate::windows_screen_capture::TargetCaptureSession::start(
            token,
            indicator_mode,
            move |frame| {
                if callback_first_frame_sent
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    let _ = first_frame_tx.try_send((frame.width, frame.height));
                }
                *callback_latest_frame.lock_unpoisoned() = Some(frame);
                callback_frame_ready.notify_one();
            },
        )
    });

    let started = match start_task.await {
        Ok(Ok(started)) => started,
        Ok(Err(error)) => {
            crate::region_window::release_selector_capture_exclusion(selector_capture_exclusion);
            crate::windows_share_overlay::close_share_overlay(&app, token);
            return Err(ShareError::Capture(error));
        }
        Err(error) => {
            crate::region_window::release_selector_capture_exclusion(selector_capture_exclusion);
            crate::windows_share_overlay::close_share_overlay(&app, token);
            return Err(ShareError::Capture(format!(
                "share capture task failed: {error}"
            )));
        }
    };
    let (capture, status) = started;
    let first_frame = match tokio::task::spawn_blocking(move || {
        first_frame_rx.recv_timeout(SHARE_FIRST_FRAME_TIMEOUT)
    })
    .await
    {
        Ok(result) => result,
        Err(error) => {
            drop_share_capture(capture).await;
            crate::region_window::release_selector_capture_exclusion(selector_capture_exclusion);
            crate::windows_share_overlay::close_share_overlay(&app, token);
            return Err(ShareError::Capture(format!(
                "share first-frame wait task failed: {error}"
            )));
        }
    };
    let (width, height) = match first_frame {
        Ok(dimensions) => dimensions,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            let error = status.terminal_error().unwrap_or_else(|| {
                format!(
                    "no capture frames arrived within {}s",
                    SHARE_FIRST_FRAME_TIMEOUT.as_secs()
                )
            });
            drop_share_capture(capture).await;
            crate::region_window::release_selector_capture_exclusion(selector_capture_exclusion);
            crate::windows_share_overlay::close_share_overlay(&app, token);
            return Err(ShareError::Capture(error));
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            let error = status
                .terminal_error()
                .unwrap_or_else(|| "share capture callback stopped before first frame".to_string());
            drop_share_capture(capture).await;
            crate::region_window::release_selector_capture_exclusion(selector_capture_exclusion);
            crate::windows_share_overlay::close_share_overlay(&app, token);
            return Err(ShareError::Capture(error));
        }
    };

    let published = match room_connection
        .publish_window_at(
            width,
            height,
            crate::transport::publisher::ShareQuality::Full,
            Some(token),
        )
        .await
    {
        Ok(published) => Arc::new(published),
        Err(error) => {
            drop_share_capture(capture).await;
            crate::region_window::release_selector_capture_exclusion(selector_capture_exclusion);
            crate::windows_share_overlay::close_share_overlay(&app, token);
            return Err(ShareError::Capture(format!(
                "window share publish failed: {error}"
            )));
        }
    };

    let audio_source = AudioSourceKey::for_share(
        kind,
        (kind == SharedSourceKind::Window)
            .then(|| target.owner_process_id())
            .filter(|pid| *pid != 0),
    );
    if let Some(source) = audio_source {
        // Audio is best-effort and starts only after the visual publication is
        // healthy; a failed process loopback never substitutes system audio.
        acquire_screen_audio_source(
            &app,
            state,
            &room_connection,
            &generation,
            audio_share_id,
            source,
        )
        .await;
    } else if kind == SharedSourceKind::Window {
        log::warn!(
            "windows session: share token={token} has no valid owner process for screen audio; continuing video-only"
        );
    }

    let title = share_title_for_target(target);
    // Kind/title metadata is REQUIRED for display-share interop: receivers
    // read `shared_window_title_from_metadata` to title the compositor
    // window and know whether this is a display share.
    room_connection
        .set_shared_window_info(
            token,
            title.clone(),
            1.0,
            source_url.clone(),
            VideoColorProfile::SRGB_BT709_FULL,
            kind,
            Some(share_instance_id.clone()),
        )
        .await;
    // Publish the sharer-chosen control mode (receiver header reads it
    // read-only). Host-side authority only.
    room_connection
        .set_shared_control_mode(token, control_mode)
        .await;
    // Local replay gate: the mode routes cursor-preserving vs full-control.
    crate::windows_remote_control::set_share_mode(token, control_mode);

    let shared = Arc::new(SharePumpShared {
        published: Mutex::new(published),
    });
    let url_refresh = (kind == SharedSourceKind::Window).then(|| {
        start_share_url_refresh(
            room_connection.clone(),
            generation.clone(),
            target,
            token,
            title.clone(),
            source_url.clone(),
            share_instance_id.clone(),
        )
    });
    let pump = start_share_frame_pump(
        app.clone(),
        generation.clone(),
        latest_frame,
        frame_ready,
        shared.clone(),
        token,
        kind,
    );
    let loss_monitor = start_share_loss_monitor(
        app.clone(),
        room_connection.clone(),
        generation.clone(),
        status.clone(),
        token,
    );

    let mut share = Some(ActiveShare {
        capture,
        status: status.clone(),
        pump,
        url_refresh,
        token,
        kind,
        title,
        shared,
        share_instance_id,
        control_mode,
        allow_remote_control: AtomicBool::new(state.remote_control_policy().allows_requests()),
        selector_capture_exclusion,
        frame: Mutex::new(
            crate::windows_capture_target::resolve(token)
                .ok()
                .map(|target| {
                    current_target_frame(
                        target,
                        crate::platform::cg::WindowFrame {
                            x: 0,
                            y: 0,
                            width: 0,
                            height: 0,
                        },
                    )
                })
                .unwrap_or(crate::platform::cg::WindowFrame {
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 0,
                }),
        ),
        audio_source,
        audio_share_id,
    });

    let _committed = {
        let mut joined = state.joined.lock_unpoisoned();
        let current = generation.is_current()
            && joined
                .as_ref()
                .map(|session| {
                    Arc::ptr_eq(&session.room_connection, &room_connection)
                        && !session
                            .media
                            .shares
                            .iter()
                            .any(|share| share.token == token)
                })
                .unwrap_or(false);
        if current {
            joined
                .as_mut()
                .expect("current media session")
                .media
                .shares
                .push(share.take().expect("pending share"));
        }
        current
    };

    if let Some(share) = share {
        // Lost the commit race (left the room / duplicate start): tear down.
        let ActiveShare {
            capture,
            pump,
            url_refresh,
            shared,
            selector_capture_exclusion,
            audio_source,
            audio_share_id,
            ..
        } = share;
        if let Some(source) = audio_source {
            release_screen_audio_source(
                &app,
                state,
                Some(&room_connection),
                audio_share_id,
                source,
            )
            .await;
        }
        pump.abort();
        if let Some(url_refresh) = url_refresh {
            url_refresh.abort();
        }
        loss_monitor.abort();
        drop_share_capture(capture).await;
        crate::region_window::release_selector_capture_exclusion(selector_capture_exclusion);
        let published = shared.published.lock_unpoisoned().clone();
        let _ = published.unpublish().await;
        crate::windows_share_overlay::close_share_overlay(&app, token);
        let _ = identity;
        return Err(ShareError::Unknown(
            "share start lost the room commit race".to_string(),
        ));
    }

    if is_region_share {
        crate::region_window::set_active_share(token, true);
    }
    emit_share_state_changed(&app, token, true);
    crate::analytics::share_started(match kind {
        SharedSourceKind::Window => crate::analytics::ShareStartedSource::Window,
        SharedSourceKind::Display | SharedSourceKind::DisplayRegion => {
            crate::analytics::ShareStartedSource::Display
        }
    });
    Ok(true)
}

fn start_share_url_refresh(
    room_connection: Arc<RoomConnection>,
    generation: RoomGeneration,
    target: crate::windows_capture_target::WindowsCaptureTarget,
    token: u32,
    title: String,
    mut current_url: Option<String>,
    share_instance_id: String,
) -> tauri::async_runtime::JoinHandle<()> {
    tauri::async_runtime::spawn(async move {
        // File Explorer and other non-browser windows must not pay for a
        // repeated accessibility walk. The one-time process check is outside
        // the share-start path and leaves the existing media share untouched.
        if !crate::browser_url::windows_target_supports_url_extraction(target).await {
            return;
        }
        let mut interval = tokio::time::interval(SHARE_URL_REFRESH_INTERVAL);
        interval.tick().await;
        loop {
            interval.tick().await;
            if !generation.is_current() {
                break;
            }
            let next_url = crate::browser_url::url_for_windows_target(target).await;
            if next_url == current_url {
                continue;
            }
            room_connection
                .set_shared_window_info(
                    token,
                    title.clone(),
                    1.0,
                    next_url.clone(),
                    VideoColorProfile::SRGB_BT709_FULL,
                    SharedSourceKind::Window,
                    Some(share_instance_id.clone()),
                )
                .await;
            current_url = next_url;
        }
    })
}

/// Build the wire `CapturedFrame` for a captured BGRA frame (used by both
/// the live push path and the idle-refresh re-push).
fn share_captured_frame(
    frame: &crate::windows_screen_capture::BgraFrame,
    sequence: u64,
) -> crate::capture::CapturedFrame {
    crate::capture::CapturedFrame {
        width: frame.width,
        height: frame.height,
        payload: crate::capture::CapturedFramePayload::Bgra {
            data: crate::capture::PooledFrameData::from_vec(frame.bgra.clone()),
            bytes_per_row: frame.bytes_per_row,
        },
        source_scale: 1.0,
        layout_validated: true,
        color_profile: VideoColorProfile::SRGB_BT709_FULL,
        sequence,
        dirty_rect_count: 0,
        dirty_area_px: 0,
        dirty_rects_known: false,
        lock_copy_ms: 0.0,
        region_generation: frame.region_generation,
    }
}

/// Latest-wins frame pump: take the newest captured frame and push it into
/// the published track. The publisher letterboxes the frame to a fixed
/// published size while a resize is in progress (encoded size constant —
/// webrtc re-creates the encoder on any size change and that churn breaks
/// the MF encoder) and re-anchors the published size once the window
/// settles (~2s). No track republish on Windows. All Windows shares publish
/// at `ShareQuality::Full`.
///
/// Every silent state is logged: the first push, a minimized-window hold
/// (frames resume on restore), and a per-interval push cadence so a
/// capture that delivers no frames is diagnosable from petal.log instead of
/// looking like a share that "stopped". Static content (WGC delivers nothing
/// after the initial frame) is kept alive by the idle-refresh re-push.
fn start_share_frame_pump(
    app: tauri::AppHandle,
    generation: RoomGeneration,
    latest_frame: Arc<Mutex<Option<crate::windows_screen_capture::BgraFrame>>>,
    frame_ready: Arc<tokio::sync::Notify>,
    shared: Arc<SharePumpShared>,
    token: u32,
    kind: SharedSourceKind,
) -> tauri::async_runtime::JoinHandle<()> {
    tauri::async_runtime::spawn(async move {
        let mut sequence = 0u64;
        let mut first_frame_logged = false;
        let mut minimized_hold_logged = false;
        let mut pushed_this_interval = 0u64;
        let mut accepted_this_interval = 0u64;
        let mut push_failed_this_interval = 0u64;
        let mut source_rejected_this_interval = 0u64;
        let mut convert_ms_this_interval = 0.0f64;
        let mut capture_return_ms_this_interval = 0.0f64;
        let mut max_capture_age_ms_this_interval = 0.0f64;
        let mut max_convert_ms_this_interval = 0.0f64;
        let mut max_capture_return_ms_this_interval = 0.0f64;
        // Last successfully pushed frame: re-pushed by the idle-refresh timer
        // when WGC goes silent on static content (see the constant's doc).
        let mut last_pushed: Option<crate::windows_screen_capture::BgraFrame> = None;
        let mut last_push_at = std::time::Instant::now();
        let mut health = tokio::time::interval(SHARE_PUMP_HEALTH_INTERVAL);
        health.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let pump_started_at = std::time::Instant::now();
        let mut last_region_warning = None;
        loop {
            sync_region_warning(&app, token, &mut last_region_warning);
            // WGC is change-driven; during a remote-control input window the
            // pump re-pushes at the boost cadence so the receiver stream stays
            // hot and each real WGC frame is delivered promptly. The same
            // boost cadence applies for the first `SHARE_INITIAL_BURST_WINDOW`
            // after share start, so a subscriber attaching during the SFU's
            // publish propagation gets its keyframe request answered within
            // one frame-time instead of waiting out the idle-refresh interval.
            let refresh_interval = if rc_boost_active(token)
                || pump_started_at.elapsed() < SHARE_INITIAL_BURST_WINDOW
            {
                RC_FPS_BOOST_REFRESH_INTERVAL
            } else {
                SHARE_IDLE_REFRESH_INTERVAL
            };
            tokio::select! {
                _ = health.tick() => {
                    if !generation.is_current() {
                        break;
                    }
                    log::debug!(
                        "[DEBUG-MEDIA-FPS] windows session: share {token} pump_pushed={pushed_this_interval} accepted={accepted_this_interval} frame(s) in the last {}s ({:.1} fps), push_failed={push_failed_this_interval} source_rejected={source_rejected_this_interval} avg_convert_ms={:.2} avg_capture_frame_return_ms={:.2} max_capture_age_ms={:.1} max_convert_ms={:.2} max_capture_frame_return_ms={:.2}",
                        SHARE_PUMP_HEALTH_INTERVAL.as_secs(),
                        pushed_this_interval as f64 / SHARE_PUMP_HEALTH_INTERVAL.as_secs() as f64,
                        if pushed_this_interval > 0 { convert_ms_this_interval / pushed_this_interval as f64 } else { 0.0 },
                        if pushed_this_interval > 0 { capture_return_ms_this_interval / pushed_this_interval as f64 } else { 0.0 },
                        max_capture_age_ms_this_interval,
                        max_convert_ms_this_interval,
                        max_capture_return_ms_this_interval,
                    );
                    pushed_this_interval = 0;
                    accepted_this_interval = 0;
                    push_failed_this_interval = 0;
                    source_rejected_this_interval = 0;
                    convert_ms_this_interval = 0.0;
                    capture_return_ms_this_interval = 0.0;
                    max_capture_age_ms_this_interval = 0.0;
                    max_convert_ms_this_interval = 0.0;
                    max_capture_return_ms_this_interval = 0.0;
                    continue;
                }
                _ = tokio::time::sleep(refresh_interval) => {
                    if !generation.is_current() {
                        break;
                    }
                    // WGC is change-driven: a visually static window stops
                    // delivering frames after the initial content. Re-push
                    // the last frame so receivers keep receiving (macOS
                    // `idle_static_refresh` parity; macOS receivers retire
                    // windows whose stream goes silent).
                    let Some(previous) = last_pushed.as_ref() else {
                        continue;
                    };
                    if kind == SharedSourceKind::DisplayRegion
                        && !region_frame_generation_is_current(token, previous.region_generation)
                    {
                        last_pushed = None;
                        continue;
                    }
                    if kind == SharedSourceKind::Window && share_target_is_minimized(token) {
                        continue;
                    }
                    if last_push_at.elapsed() < refresh_interval {
                        // New frames are flowing, or we're inside the boost
                        // cadence; nothing to refresh.
                        continue;
                    }
                    sequence += 1;
                    let captured = share_captured_frame(previous, sequence);
                    let published = shared.published.lock_unpoisoned().clone();
                    if let Some(timing) =
                        published.push_frame(&captured, previous.capture_wall_time_us)
                    {
                        pushed_this_interval += 1;
                        convert_ms_this_interval += timing.convert_ms;
                        capture_return_ms_this_interval += timing.capture_frame_return_ms;
                        max_capture_age_ms_this_interval =
                            max_capture_age_ms_this_interval.max(timing.capture_age_ms);
                        max_convert_ms_this_interval =
                            max_convert_ms_this_interval.max(timing.convert_ms);
                        max_capture_return_ms_this_interval = max_capture_return_ms_this_interval
                            .max(timing.capture_frame_return_ms);
                        if timing.source_accepted {
                            accepted_this_interval += 1;
                        } else {
                            source_rejected_this_interval += 1;
                        }

                        last_push_at = std::time::Instant::now();
                        log::debug!(
                            "windows session: share {token} idle-refresh pushed last frame (static content)"
                        );
                    } else {
                        push_failed_this_interval += 1;
                    }
                    continue;
                }
                _ = frame_ready.notified() => {}
            }
            if !generation.is_current() {
                break;
            }
            let frame = latest_frame.lock_unpoisoned().take();
            let Some(frame) = frame else {
                continue;
            };
            if kind == SharedSourceKind::DisplayRegion
                && !region_frame_generation_is_current(token, frame.region_generation)
            {
                log::debug!("windows session: share {token} dropped stale GPU ROI frame");
                continue;
            }
            if kind == SharedSourceKind::Window && share_target_is_minimized(token) {
                // Hold the last frame while minimized; resume on restore.
                if !minimized_hold_logged {
                    minimized_hold_logged = true;
                    log::warn!(
                        "windows session: share {token} HOLDING last frame: window is minimized; frames resume on restore"
                    );
                }
                continue;
            }
            if minimized_hold_logged {
                minimized_hold_logged = false;
                log::info!("windows session: share {token} resumed pushing (window restored)");
            }
            sequence += 1;
            if !first_frame_logged {
                first_frame_logged = true;
                log::info!(
                    "windows session: share {token} pushed first frame {}x{}",
                    frame.width,
                    frame.height
                );
            }
            let captured = share_captured_frame(&frame, sequence);
            let published = shared.published.lock_unpoisoned().clone();
            let push_result = published.push_frame(&captured, frame.capture_wall_time_us);
            if let Some(timing) = push_result {
                pushed_this_interval += 1;
                convert_ms_this_interval += timing.convert_ms;
                capture_return_ms_this_interval += timing.capture_frame_return_ms;
                max_capture_age_ms_this_interval =
                    max_capture_age_ms_this_interval.max(timing.capture_age_ms);
                max_convert_ms_this_interval = max_convert_ms_this_interval.max(timing.convert_ms);
                max_capture_return_ms_this_interval =
                    max_capture_return_ms_this_interval.max(timing.capture_frame_return_ms);
                if timing.source_accepted {
                    accepted_this_interval += 1;
                } else {
                    source_rejected_this_interval += 1;
                }
                last_pushed = Some(frame);
                last_push_at = std::time::Instant::now();
                continue;
            }
            // push_frame returned None for a non-size reason (source teardown
            // etc.); drop the frame and keep the pump alive. Resizes are
            // handled inside the publisher: frames are letterboxed to the
            // published size during the gesture and the size is re-anchored
            // once it settles — no track republish on Windows.
            push_failed_this_interval += 1;
            continue;
        }
    })
}

fn sync_region_warning(app: &tauri::AppHandle, token: u32, last: &mut Option<bool>) {
    let Some(source) = crate::region_window::resolve(token) else {
        return;
    };
    if *last == Some(source.outside_display) {
        return;
    }
    *last = Some(source.outside_display);
    // Route by the selector's native window label, NOT the capture token:
    // tokens share the capture-target counter and diverge from the label
    // number as soon as more than one Petal View exists (015A: token 6 vs
    // "region-window-2"), so token-keyed events reached the wrong window.
    let payload = serde_json::json!({
        "windowId": token,
        "selectorLabel": crate::region_window::selector_label_from_title(&source.title),
        "outsideDisplay": source.outside_display,
    });
    if let Err(error) = tauri::Emitter::emit(app, "region-warning", payload) {
        log::debug!("windows session: region warning emit failed for {token}: {error}");
    }
}

fn region_frame_generation_is_current(token: u32, generation: Option<u64>) -> bool {
    let Some(source) = crate::region_window::resolve(token) else {
        return false;
    };
    generation == Some(source.generation.0)
}

/// Terminal-capture watchdog: stops the share when the WGC capture reports a
/// terminal error (source closed, frame pipeline failure). Self-terminates
/// once the share is no longer registered (generation change or an explicit
/// stop).
fn start_share_loss_monitor(
    app: tauri::AppHandle,
    room_connection: Arc<RoomConnection>,
    generation: RoomGeneration,
    status: crate::windows_screen_capture::CaptureStatus,
    token: u32,
) -> tauri::async_runtime::JoinHandle<()> {
    tauri::async_runtime::spawn(async move {
        let mut interval = tokio::time::interval(SHARE_LOSS_POLL_INTERVAL);
        loop {
            interval.tick().await;
            if !generation.is_current() {
                break;
            }
            let Some(error) = status.terminal_error() else {
                continue;
            };
            let Some(state) = app.try_state::<SessionState>() else {
                break;
            };
            let _control = state.lock_share_control().await;
            if !generation.is_current() {
                break;
            }
            let share = {
                let mut joined = state.joined.lock_unpoisoned();
                joined.as_mut().and_then(|session| {
                    let position = session
                        .media
                        .shares
                        .iter()
                        .position(|share| share.token == token)?;
                    Some(session.media.shares.remove(position))
                })
            };
            let Some(share) = share else {
                break;
            };
            stop_share(&app, share, room_connection.clone()).await;
            crate::analytics::share_stopped(crate::analytics::ShareStoppedReason::CaptureFailed);
            emit_share_state_changed(&app, token, false);
            emit_share_error(&app, token, false, ShareError::Capture(error));
            break;
        }
    })
}

async fn stop_share(
    app: &tauri::AppHandle,
    share: ActiveShare,
    room_connection: Arc<RoomConnection>,
) {
    let ActiveShare {
        capture,
        pump,
        url_refresh,
        token,
        kind,
        shared,
        selector_capture_exclusion,
        audio_source,
        audio_share_id,
        ..
    } = share;
    let started = std::time::Instant::now();
    if let Some(source) = audio_source {
        // Audio signaling/native stop is independent of the visual lifecycle;
        // do not hold remote-window teardown behind it. The detached release
        // still uses the source-control lock before any replacement starts.
        spawn_screen_audio_release(app, Some(room_connection.clone()), audio_share_id, source);
    }
    pump.abort();
    if let Some(url_refresh) = url_refresh {
        url_refresh.abort();
    }
    // Cancel hover placement before retiring the source. This releases the
    // follower's drag freeze and restores the committed position before any
    // replacement token can be published.
    #[cfg(target_os = "windows")]
    if crate::hover_core::active_hover_tab_drag(token).is_some() {
        crate::windows_hover::cancel_drag_for_lifecycle();
    }
    // Retire input authority before either teardown await. A replay task may
    // already be queued; the Windows adapter re-resolves this opaque token
    // immediately before injection and must see it as stale. If this is the
    // currently presented ordinary hover target, mint a fresh token for the
    // tracker instead of forcing an outside tab through hide/reacquire.
    crate::remote_control::revoke_window(app, token, "share stopped");
    if kind == SharedSourceKind::DisplayRegion {
        crate::region_window::set_active_share(token, false);
    }
    let keep_hover_target = kind == SharedSourceKind::Window
        && crate::hover_core::current_hover_presentation()
            .is_some_and(|presentation| presentation.window_id == token);
    let hover_replacement = keep_hover_target
        .then(|| crate::windows_capture_target::retire_for_hover(token))
        .flatten();
    #[cfg(target_os = "windows")]
    if let Some(replacement_token) = hover_replacement {
        // Update the native follower registration immediately. The cursor
        // poll will consume the bounded registry handoff later, but it must
        // never have to pass through a stale-token window first.
        let _ = crate::windows_hover::replace_hover_tab_follower_token(token, replacement_token);
    }
    let replaced_for_hover = hover_replacement.is_some();
    if replaced_for_hover {
        log::debug!(
            "windows session: share {} target token replaced for active hover tab",
            token
        );
    } else if !crate::windows_capture_target::invalidate(token) {
        log::debug!(
            "windows session: share {} target token was already invalidated",
            token
        );
    }
    // WGC must be fully dropped before the selector becomes captureable again;
    // otherwise a final frame could contain the selector's own chrome.
    drop_share_capture(capture).await;
    crate::region_window::release_selector_capture_exclusion(selector_capture_exclusion);
    log::info!(
        "windows session: share {} capture dropped ({:?})",
        token,
        started.elapsed()
    );
    let published = shared.published.lock_unpoisoned().clone();
    match published.unpublish().await {
        Ok(()) => log::info!(
            "windows session: share {} unpublished ({:?})",
            token,
            started.elapsed()
        ),
        Err(error) => {
            log::warn!("windows session: share unpublish failed (room already closed?): {error}")
        }
    }
    room_connection.clear_shared_window_title(token).await;
    crate::windows_share_overlay::close_share_overlay(app, token);
    log::info!(
        "windows session: share {} fully stopped ({:?})",
        token,
        started.elapsed()
    );
}

pub(crate) async fn stop_share_token(
    app: &tauri::AppHandle,
    state: &SessionState,
    token: u32,
) -> Result<(), ShareError> {
    log::info!("windows session: stop_share_token begin for token {token}");
    let (share, room_connection) = {
        let mut joined = state.joined.lock_unpoisoned();
        let session = joined.as_mut().ok_or_else(|| ShareError::NotInRoom)?;
        let position = session
            .media
            .shares
            .iter()
            .position(|share| share.token == token)
            .ok_or_else(|| ShareError::WindowNotFound(token.to_string()))?;
        let share = session.media.shares.remove(position);
        (share, session.room_connection.clone())
    };
    stop_share(app, share, room_connection).await;
    crate::analytics::share_stopped(crate::analytics::ShareStoppedReason::User);
    // Drop the local mode gate so a stale packet after teardown refuses.
    crate::windows_remote_control::clear_share_mode(token);
    log::info!("windows session: stop_share_token done for token {token}");
    Ok(())
}

/// Toggle: returns true when now shared, false when now unshared. The
/// frontend's `WindowPicker.toggleShare` and the meeting route's
/// `handleScreenshareControl` depend on this contract.
#[tauri::command]
pub async fn share_window(
    app: tauri::AppHandle,
    state: tauri::State<'_, SessionState>,
    window_id: u32,
    color: Option<String>,
    control_mode: Option<String>,
) -> Result<bool, ShareError> {
    let color = crate::hover_core::share_color_or_default(color.as_deref());
    let mode =
        crate::remote_control_core::RemoteControlMode::from_wire_option(control_mode.as_deref());
    let _control = state.lock_share_control().await;
    if state.shared_window_ids().contains(&window_id) {
        stop_share_token(&app, &state, window_id).await?;
        emit_share_state_changed(&app, window_id, false);
        return Ok(false);
    }
    match start_share_token(app.clone(), &state, window_id, mode, color).await {
        Ok(true) => Ok(true),
        Ok(false) => Ok(true),
        Err(error) => {
            emit_share_error(&app, window_id, true, error.clone());
            Err(error)
        }
    }
}

/// Apply a host-owned live control-mode change to an existing share. Kept
/// separate from the Tauri wrapper so the consent path can revalidate an
/// escalation and invoke the same authoritative mutation without fabricating
/// a `tauri::State` value.
pub(crate) async fn set_share_control_mode_for_window(
    app: &tauri::AppHandle,
    state: &SessionState,
    window_id: u32,
    requested_mode: crate::remote_control_core::RemoteControlMode,
) -> Result<(), ShareError> {
    let _control = state.lock_share_control().await;
    let connection = {
        let joined = state.joined.lock_unpoisoned();
        joined
            .as_ref()
            .ok_or(ShareError::NotInRoom)?
            .room_connection
            .clone()
    };
    let mode = {
        let mut joined = state.joined.lock_unpoisoned();
        let Some(session) = joined.as_mut() else {
            return Err(ShareError::NotInRoom);
        };
        let Some(share) = session
            .media
            .shares
            .iter_mut()
            .find(|share| share.token == window_id)
        else {
            return Err(ShareError::WindowNotFound(window_id.to_string()));
        };
        let target_kind = match share.kind {
            SharedSourceKind::Window => TargetKind::Window,
            SharedSourceKind::Display | SharedSourceKind::DisplayRegion => TargetKind::Display,
        };
        let mode = effective_control_mode(target_kind, requested_mode);
        share.control_mode = mode;
        mode
    };
    // A manual host mode change invalidates any pending escalation prompt; a
    // queued approval must never race a newer mode decision.
    #[cfg(target_os = "windows")]
    crate::remote_control::clear_escalations_for_window(window_id);
    // Local replay gate + published metadata for the receiver header.
    #[cfg(target_os = "windows")]
    crate::windows_remote_control::set_share_mode(window_id, mode);
    connection.set_shared_control_mode(window_id, mode).await;
    let control_mode = match mode {
        crate::remote_control_core::RemoteControlMode::CursorPreserving => "cursorPreserving",
        crate::remote_control_core::RemoteControlMode::FullControl => "fullControl",
        crate::remote_control_core::RemoteControlMode::Unknown => "cursorPreserving",
    };
    let _ = tauri::Emitter::emit(
        app,
        "share-control-mode-changed",
        serde_json::json!({ "windowId": window_id, "controlMode": control_mode }),
    );
    log::info!(
        "windows session: share control mode changed window={window_id} mode={control_mode}"
    );
    Ok(())
}

/// Live-share control-mode toggle. The sharer can raise (or lower) the mode
/// of an already-running share; the controller needs no change (host-side
/// policy). Display shares remain FullControl regardless of the requested
/// mode; unknown/None keeps the window default cursor-preserving.
#[tauri::command]
pub async fn set_share_control_mode(
    app: tauri::AppHandle,
    state: tauri::State<'_, SessionState>,
    window_id: u32,
    control_mode: Option<String>,
) -> Result<(), ShareError> {
    let requested_mode =
        crate::remote_control_core::RemoteControlMode::from_wire_option(control_mode.as_deref());
    set_share_control_mode_for_window(&app, state.inner(), window_id, requested_mode).await
}

/// The on-screen region WGC actually captures for a shared window: the
/// window's true visible content minus the invisible DWM resize borders.
/// The item-vs-client relationship INVERTS by chrome style (observed live):
///   - captioned windows (title bar): `GetClientRect` INCLUDES the invisible
///     resize borders, so the capture = client minus them (GitHub Desktop:
///     client 1484x858 -> item 1470x851, origin = client.x + 7);
///   - borderless windows (RUN): `GetClientRect` is the pure drawing area,
///     the capture = the OUTER rect minus the same invisible borders (RUN:
///     outer 1722x977 -> item 1708x970, origin = outer.x + 7).
fn effective_control_mode(
    target_kind: TargetKind,
    requested: crate::remote_control_core::RemoteControlMode,
) -> crate::remote_control_core::RemoteControlMode {
    if target_kind == TargetKind::Display {
        crate::remote_control_core::RemoteControlMode::FullControl
    } else {
        requested
    }
}

fn region_content_frame(window_id: u32) -> Option<crate::platform::cg::WindowFrame> {
    let source = crate::region_window::resolve(window_id)?;
    Some(crate::platform::cg::WindowFrame {
        x: source.frame.x.round() as i32,
        y: source.frame.y.round() as i32,
        width: source.frame.width.round() as i32,
        height: source.frame.height.round() as i32,
    })
}

fn current_target_frame(
    target: crate::windows_capture_target::WindowsCaptureTarget,
    fallback: crate::platform::cg::WindowFrame,
) -> crate::platform::cg::WindowFrame {
    match target.kind() {
        TargetKind::Window => {
            crate::platform::windows::window_frame_for_raw(target.raw_handle()).unwrap_or(fallback)
        }
        TargetKind::Display => {
            crate::platform::windows::display_frame_for_raw(target.raw_handle()).unwrap_or(fallback)
        }
    }
}

/// The chrome is detected from the outer-vs-client top gap (the title bar),
/// and the border width is measured from the deltas — DPI-correct without
/// hardcoding 7px. Maximized windows (no invisible borders) fall through to
/// the client rect. Falls back to the client rect when the capture size is
/// unknown (share not yet pumping frames).
fn telepointer_content_frame(
    client: crate::platform::cg::WindowFrame,
    outer: crate::platform::cg::WindowFrame,
    size: Option<(u32, u32)>,
) -> crate::platform::cg::WindowFrame {
    let Some((item_w, item_h)) = size.filter(|(w, h)| *w > 0 && *h > 0) else {
        return client;
    };
    let item_w = item_w as i32;
    let item_h = item_h as i32;
    let captioned = client.y > outer.y;
    if captioned {
        let inset = (client.width - item_w) / 2;
        if inset >= 0 {
            // DWM invisible resize borders: the capture excludes them, so the
            // content is the client minus the (symmetric side, top) insets.
            return crate::platform::cg::WindowFrame {
                x: client.x + inset,
                y: client.y,
                width: item_w,
                height: item_h,
            };
        }
    } else {
        let inset = (outer.width - item_w) / 2;
        if inset >= 0 {
            return crate::platform::cg::WindowFrame {
                x: outer.x + inset,
                y: outer.y,
                width: item_w,
                height: item_h,
            };
        }
    }
    // The capture is LARGER than the client/outer frame: the WGC item
    // includes the window's visible border, i.e. it covers the full
    // GetWindowRect region. Normalizing against the client here put the tag
    // ~1-2px off the real cursor (observed live on the RUN window:
    // client=(94, 41, 1706x969), item=(1708, 970), capture anchored at the
    // outer rect) — anchor the item at the outer origin instead.
    crate::platform::cg::WindowFrame {
        x: outer.x,
        y: outer.y,
        width: item_w,
        height: item_h,
    }
}

/// Current share tokens (window AND display).
#[tauri::command]
pub fn shared_window_ids(state: tauri::State<'_, SessionState>) -> Vec<u32> {
    state.shared_window_ids()
}

#[tauri::command]
pub async fn join_room_command(
    app: tauri::AppHandle,
    rooms: tauri::State<'_, crate::rooms::RoomsState>,
    state: tauri::State<'_, SessionState>,
    room_name: String,
    identity: String,
    display_name: String,
    remote_control_allowed: bool,
    remote_control_policy: Option<RemoteControlPolicy>,
    identity_palette_index: Option<u8>,
) -> Result<crate::rooms::RoomRecord, SessionError> {
    let remote_control_policy = remote_control_policy.unwrap_or_else(|| {
        RemoteControlPolicy::from_allowed(remote_control_allowed, RemoteControlPolicy::Ask)
    });
    let _transition = state.transition_lock.lock().await;
    let room_record = crate::meeting_core::persist_joined_room_record(&rooms, &room_name)?;

    {
        let joined = state.joined.lock_unpoisoned();
        if let Some(joined) = joined.as_ref() {
            if is_same_room(&joined.room_record, &room_record) {
                log::info!(
                    "windows session: already joined '{}'; treating rejoin as a no-op",
                    crate::logging::log_safe_quoted(&room_record.name)
                );
                return Ok(joined.room_record.clone());
            }
        }
    }

    leave_room_inner(&app, &state, true, "room_switch").await;

    let generation = state.begin_room_generation();
    let connected = match crate::meeting_core::connect_room(
        &rooms,
        room_record,
        &identity,
        &display_name,
    )
    .await
    {
        Ok(connected) => connected,
        Err(error) => {
            state.invalidate_room_generation();
            return Err(error.into());
        }
    };
    let room_record = connected.room_record;
    let room_connection = connected.room_connection;
    // Windows telepointer receiver: name-tagged remote cursors over our
    // compositor windows (same seam as macOS session/room.rs).
    crate::telepointer::start_receiver_for_room(
        &app,
        room_connection.room().clone(),
        generation.clone(),
    );
    crate::draw::start_receiver_for_room(&app, room_connection.room().clone(), generation.clone());
    // Plugin data bus (plugins/README.md §2.6), Windows parity with
    // session/room.rs: without this the bus is send-only here -- publishes go
    // out but no `plugin-data` / `plugin-state-changed` event ever arrives.
    crate::plugins::start_receiver_for_room(
        &app,
        room_connection.room().clone(),
        generation.clone(),
    );
    crate::remote_control::start_receiver_for_room(
        &app,
        room_connection.room().clone(),
        identity.clone(),
        generation.clone(),
    );
    // AI chat (#657, Windows parity): start/stop requests, push-to-talk floor
    // claims, and remote session state over `petal.ai-chat`. Same seam as the
    // macOS session/room.rs wiring; the receiver stops when the room
    // generation is invalidated on leave.
    crate::ai_chat::topic::start_receiver_for_room(
        &app,
        room_connection.room().clone(),
        generation.clone(),
    );
    let livekit_room_name = connected.livekit_room_name;
    match room_connection
        .publish_identity_palette_index(identity_palette_index)
        .await
    {
        Ok(palette_index) if generation.is_current() => {
            room_connection.commit_identity_palette_index(palette_index);
        }
        Ok(_) => log::warn!("windows session: skipped stale identity palette commit"),
        Err(error) => {
            log::warn!("windows session: failed to publish identity color metadata: {error}");
        }
    }
    let presence = Arc::new(crate::presence::PresenceState::default());
    let (watcher_cancellation, watcher_cancelled) = media_watcher_cancellation();
    let compositor_events = match room_connection.take_compositor_events() {
        Some(events) => Some(events),
        None => {
            log::warn!("windows session: connect-time event receiver was already consumed");
            None
        }
    };
    // Keep the resilience branch for the independent screen-audio
    // publication repair watcher. The compositor still owns the other branch
    // and the watcher exits on the room's Disconnected event.
    let resilience_events = room_connection.take_resilience_events();

    state.seed_remote_control_policy(remote_control_policy);
    {
        let mut joined = state.joined.lock_unpoisoned();
        *joined = Some(WindowsMediaSession {
            room_record: room_record.clone(),
            identity: identity.clone(),
            room_connection: room_connection.clone(),
            presence: presence.clone(),
            media: MediaResources::default(),
            watcher_cancellation: Some(watcher_cancellation),
        });
    }

    crate::presence::start_for_room(
        &app,
        room_connection.room(),
        presence,
        room_record.name.clone(),
        identity.clone(),
        display_name,
        generation.clone(),
    );

    // The compositor feed REPLACES `start_disconnect_watcher` as the consumer
    // of the connect-time event receiver; its connect-time delivery is what
    // guarantees late joiners receive already-published shares (#364). The
    // feed owns the forced-disconnect fan-out; the receiver task below moves
    // the old `start_disconnect_watcher` Disconnected arm (session teardown)
    // onto that signal.
    let (disconnect_tx, mut disconnect_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    match compositor_events {
        Some(events) => {
            crate::transport::subscriber::start_compositor_feed(
                &app,
                events,
                room_connection.room().clone(),
                identity.clone(),
                generation.clone(),
                disconnect_tx,
            );
        }
        None => drop(disconnect_tx),
    }
    {
        let app_for_disconnect = app.clone();
        let room_connection_for_disconnect = room_connection.clone();
        let generation_for_disconnect = generation.clone();
        tauri::async_runtime::spawn(async move {
            if disconnect_rx.recv().await.is_none() {
                return;
            }
            if let Some(state) = app_for_disconnect.try_state::<SessionState>() {
                generation_for_disconnect.invalidate_if_current();
                let _transition = state.transition_lock.lock().await;
                let owns_room = state
                    .joined
                    .lock_unpoisoned()
                    .as_ref()
                    .map(|session| {
                        Arc::ptr_eq(&session.room_connection, &room_connection_for_disconnect)
                    })
                    .unwrap_or(false);
                if owns_room {
                    leave_room_inner(&app_for_disconnect, &state, false, "forced_disconnect").await;
                }
            }
        });
    }
    // Network/system diagnostics (issue #19 Phase A): stats poller + event
    // journal, exactly once per room connection -- same seam as macOS
    // session/room.rs. Deliberately self-terminating (its event loop breaks
    // on RoomEvent::Disconnected; the generation counter stops the poller),
    // so leave_room needs no diagnostics teardown call.
    crate::diagnostics::start_for_room(
        &app,
        room_connection.room(),
        room_record.name.clone(),
        connected.url.clone(),
        identity.clone(),
    );
    let preferences = app.state::<crate::transport::audio::AudioDevicePreferences>();
    start_audio_for_session(
        app.clone(),
        &state,
        room_connection.clone(),
        generation.clone(),
        preferences.recording_device(),
        preferences.playout_device(),
        watcher_cancelled,
    )
    .await;
    if let Some(events) = resilience_events {
        start_screen_audio_reconnect_watcher(app.clone(), events, generation.clone());
    }
    if !generation.is_current()
        || room_connection.room().connection_state() == livekit::ConnectionState::Disconnected
    {
        leave_room_inner(&app, &state, false, "join_disconnect").await;
        return Err(SessionError::RoomConnect(
            "room disconnected during native media startup".to_string(),
        ));
    }

    log::info!(
        "windows session: joined '{}' (LiveKit room '{}') as '{}'",
        crate::logging::log_safe_quoted(&room_record.name),
        crate::logging::log_safe_quoted(&livekit_room_name),
        crate::logging::log_safe_quoted(&identity)
    );
    crate::analytics::meeting_joined();
    Ok(room_record)
}

fn start_screen_audio_reconnect_watcher(
    app: tauri::AppHandle,
    mut events: tokio::sync::mpsc::UnboundedReceiver<livekit::RoomEvent>,
    generation: RoomGeneration,
) {
    tauri::async_runtime::spawn(async move {
        while let Some(event) = events.recv().await {
            if !generation.is_current() {
                return;
            }
            match event {
                livekit::RoomEvent::Reconnected => {
                    let Some(state) = app.try_state::<SessionState>() else {
                        return;
                    };
                    repair_screen_audio_publications_after_reconnect(
                        &app,
                        state.inner(),
                        generation.clone(),
                    )
                    .await;
                }
                livekit::RoomEvent::Disconnected { .. } => return,
                _ => {}
            }
        }
    });
}

async fn repair_screen_audio_publications_after_reconnect(
    app: &tauri::AppHandle,
    state: &SessionState,
    generation: RoomGeneration,
) {
    let Some((room_connection, sources)) = state.screen_audio_repair_snapshot(&generation) else {
        return;
    };
    for (source, track) in sources {
        if !generation.is_current() {
            return;
        }
        let Some(track) = track else {
            ensure_screen_audio_source(app, state, &room_connection, &generation, source).await;
            continue;
        };
        if track.capture_failed() {
            restart_failed_screen_audio_source(app, state, generation.clone(), source, track).await;
            continue;
        }

        // Serialize the health check and republish with stop/source handoffs.
        let _serial = state.lock_screen_audio_control().await;
        if !generation.is_current()
            || !state.screen_audio_is_current(source, &track)
            || !state.screen_audio_source_is_active(source)
        {
            return;
        }
        if track.capture_failed() {
            drop(_serial);
            restart_failed_screen_audio_source(app, state, generation.clone(), source, track).await;
            continue;
        }
        let current_sid = track.track_sid().to_string();
        let expected_name = source.track_name();
        let publications: Vec<(String, String)> = room_connection
            .room()
            .local_participant()
            .track_publications()
            .values()
            .map(|publication| (publication.sid().to_string(), publication.name()))
            .collect();
        match crate::screen_audio::screen_audio_publication_health(
            &current_sid,
            &expected_name,
            publications
                .iter()
                .map(|(sid, name)| (sid.as_str(), name.as_str())),
        ) {
            crate::screen_audio::ScreenAudioPublicationHealth::CurrentSidPresent => {}
            crate::screen_audio::ScreenAudioPublicationHealth::ReplacementAlreadyPresent => {
                log::info!(
                    "windows session: screen-audio source '{}' reconnect publication already present by name",
                    source.label()
                );
            }
            crate::screen_audio::ScreenAudioPublicationHealth::Missing => {
                if let Err(error) = track.republish_after_reconnect().await {
                    log::warn!(
                        "windows session: screen-audio source '{}' reconnect publication repair failed: {error}",
                        source.label()
                    );
                }
            }
        }
    }
}

async fn start_audio_for_session(
    app: tauri::AppHandle,
    state: &SessionState,
    room_connection: Arc<RoomConnection>,
    generation: RoomGeneration,
    recording_device: Option<String>,
    playout_device: Option<String>,
    cancelled: tokio::sync::watch::Receiver<bool>,
) {
    let audio_opt_out = std::env::var("PETAL_DISABLE_AUDIO").ok();
    if audio_is_disabled(audio_opt_out.as_deref()) {
        log::warn!(
            "windows session: PETAL_DISABLE_AUDIO set -- skipping native audio (video-only run)"
        );
        return;
    }

    let initial_muted = state.mic_muted();
    let prepare_task = tokio::task::spawn_blocking(move || {
        crate::transport::audio::prepare_microphone(recording_device, initial_muted)
    });
    let prepared = match tokio::time::timeout(AUDIO_START_TIMEOUT, prepare_task).await {
        Ok(Ok(Ok(prepared))) => Some(prepared),
        Ok(Ok(Err(error))) => {
            log::warn!(
                "windows session: microphone preparation failed; continuing without mic: {error}"
            );
            None
        }
        Ok(Err(error)) => {
            log::warn!(
                "windows session: microphone preparation task failed; continuing without mic: {error}"
            );
            None
        }
        Err(_) => {
            log::warn!("windows session: microphone preparation timed out; continuing without mic");
            None
        }
    };

    if let Some(prepared) = prepared {
        let mut prepared = Some(prepared);
        let room = room_connection.room();
        match tokio::time::timeout(
            AUDIO_START_TIMEOUT,
            crate::transport::audio::publish_prepared_microphone(
                &room,
                prepared.as_ref().expect("prepared microphone present"),
            ),
        )
        .await
        {
            Ok(Ok(())) => {
                let committed = {
                    let mut joined = state.joined.lock_unpoisoned();
                    let current = generation.is_current()
                        && joined
                            .as_ref()
                            .map(|session| Arc::ptr_eq(&session.room_connection, &room_connection))
                            .unwrap_or(false);
                    if current {
                        let mic = Arc::new(
                            prepared
                                .take()
                                .expect("prepared microphone present")
                                .into_mic_track(),
                        );
                        joined
                            .as_mut()
                            .expect("current media session")
                            .media
                            .microphone = Some(mic);
                    }
                    current
                };
                if !committed {
                    let prepared = prepared.as_ref().expect("prepared microphone present");
                    prepared.mute_for_cleanup();
                    let _ = tokio::time::timeout(
                        AUDIO_CLEANUP_TIMEOUT,
                        crate::transport::audio::unpublish_prepared_microphone(&room, prepared),
                    )
                    .await;
                    return;
                }
            }
            Ok(Err(error)) => {
                log::warn!(
                    "windows session: microphone publish failed; continuing without mic: {error}"
                );
            }
            Err(_) => {
                log::warn!("windows session: microphone publish timed out; continuing without mic");
                let prepared = prepared.as_ref().expect("prepared microphone present");
                prepared.mute_for_cleanup();
                if tokio::time::timeout(
                    AUDIO_CLEANUP_TIMEOUT,
                    crate::transport::audio::unpublish_prepared_microphone(&room, prepared),
                )
                .await
                .is_err()
                {
                    log::warn!("windows session: timed-out microphone cleanup also timed out");
                }
            }
        }
    }

    let playout_task = tokio::task::spawn_blocking(move || {
        crate::transport::audio::enable_managed_playout(playout_device)
    });
    match tokio::time::timeout(AUDIO_START_TIMEOUT, playout_task).await {
        Ok(Ok(Ok(playout))) => {
            let mut joined = state.joined.lock_unpoisoned();
            let current = generation.is_current()
                && joined
                    .as_ref()
                    .map(|session| Arc::ptr_eq(&session.room_connection, &room_connection))
                    .unwrap_or(false);
            if current {
                joined
                    .as_mut()
                    .expect("current media session")
                    .media
                    .playout = Some(playout);
            }
        }
        Ok(Ok(Err(error))) => {
            log::warn!(
                "windows session: speaker playout enable failed; continuing without playout: {error}"
            );
        }
        Ok(Err(error)) => {
            log::warn!(
                "windows session: speaker playout task failed; continuing without playout: {error}"
            );
        }
        Err(_) => {
            log::warn!(
                "windows session: speaker playout enable timed out; continuing without playout"
            );
        }
    }

    let has_audio = state
        .joined
        .lock_unpoisoned()
        .as_ref()
        .map(|session| session.media.microphone.is_some() || session.media.playout.is_some())
        .unwrap_or(false);
    if has_audio {
        start_audio_device_watcher(app, generation.clone(), cancelled);
    }

    crate::transport::audio::start_audio_track_logger(room_connection.room(), generation);
}

fn start_audio_device_watcher(
    app: tauri::AppHandle,
    generation: RoomGeneration,
    mut cancelled: tokio::sync::watch::Receiver<bool>,
) {
    log::info!("windows session: audio-device watcher started");
    tauri::async_runtime::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
        let mut mic_failure_reported = false;
        let mut speaker_failure_reported = false;
        loop {
            tokio::select! {
                changed = cancelled.changed() => {
                    if changed.is_err() || *cancelled.borrow() {
                        break;
                    }
                }
                _ = interval.tick() => {
                    if !generation.is_current() {
                        break;
                    }
                    let Some(state) = app.try_state::<SessionState>() else {
                        break;
                    };
                    let _device_transaction = state.audio_device_lock.lock().await;
                    if !generation.is_current() {
                        break;
                    }
                    let (recording, playout) = state.refresh_audio_devices();
                    log::debug!(
                        "windows session: audio-device poll recording={recording:?} playout={playout:?}"
                    );
                    match recording {
                        Some(crate::transport::audio::RecordingDeviceRefresh::Unchanged) => {
                            mic_failure_reported = false;
                        }
                        Some(crate::transport::audio::RecordingDeviceRefresh::Switched(device_name)) => {
                            mic_failure_reported = false;
                            crate::analytics::device_changed(
                                crate::analytics::DeviceKind::Mic,
                                crate::analytics::DeviceChange::Switched,
                            );
                            app.state::<crate::transport::audio::AudioDevicePreferences>()
                                .set_recording_device(String::new());
                            let _ = tauri::Emitter::emit(
                                &app,
                                "resilience-event",
                                crate::resilience_event::ResilienceEvent::MicDeviceChanged {
                                    device_name,
                                    using_default: Some(true),
                                },
                            );
                        }
                        Some(crate::transport::audio::RecordingDeviceRefresh::Failed(message)) => {
                            if !mic_failure_reported {
                                mic_failure_reported = true;
                                crate::analytics::device_changed(
                                    crate::analytics::DeviceKind::Mic,
                                    crate::analytics::DeviceChange::Failed,
                                );
                                let _ = tauri::Emitter::emit(
                                    &app,
                                    "resilience-event",
                                    crate::resilience_event::ResilienceEvent::MicDeviceFailed { message },
                                );
                            }
                        }
                        None => {}
                    }
                    match playout {
                        Some(crate::transport::audio::PlayoutDeviceRefresh::Unchanged) => {
                            speaker_failure_reported = false;
                        }
                        Some(crate::transport::audio::PlayoutDeviceRefresh::Switched(device_name)) => {
                            speaker_failure_reported = false;
                            app.state::<crate::transport::audio::AudioDevicePreferences>()
                                .set_playout_device(String::new());
                            let _ = tauri::Emitter::emit(
                                &app,
                                "resilience-event",
                                crate::resilience_event::ResilienceEvent::SpeakerDeviceChanged {
                                    device_name,
                                    using_default: Some(true),
                                },
                            );
                        }
                        Some(crate::transport::audio::PlayoutDeviceRefresh::Failed(message)) => {
                            if !speaker_failure_reported {
                                speaker_failure_reported = true;
                                let _ = tauri::Emitter::emit(
                                    &app,
                                    "resilience-event",
                                    crate::resilience_event::ResilienceEvent::SpeakerDeviceFailed { message },
                                );
                            }
                        }
                        None => {}
                    }
                }
            }
        }
    });
}

#[tauri::command]
pub async fn leave_room_command(
    app: tauri::AppHandle,
    state: tauri::State<'_, SessionState>,
) -> Result<(), ()> {
    leave_room(&app, &state).await;
    Ok(())
}

pub async fn leave_room(app: &tauri::AppHandle, state: &SessionState) {
    let _transition = state.transition_lock.lock().await;
    leave_room_inner(app, state, true, "leave_room").await;
}

async fn leave_room_inner(
    app: &tauri::AppHandle,
    state: &SessionState,
    close_connection: bool,
    reason: &str,
) {
    #[cfg(target_os = "windows")]
    crate::windows_hover::cancel_drag_for_lifecycle();
    state.invalidate_room_generation();
    crate::remote_control::revoke_all(app);
    // The room credential cached for `/api/ai-token` must not outlive the
    // join (room_auth parity with the macOS leave).
    crate::ai_chat::room_auth::forget();
    // The picker is a meeting-scoped surface: it must not remain on the
    // desktop after the user exits the meeting (hide, don't destroy — a
    // re-open re-shows the hidden singleton; the window-change watcher
    // self-terminates once the picker is no longer visible).
    crate::window_picker::hide_picker_on_meeting_exit(app);
    state.set_camera_intent(false);
    // Stop the published webcam FIRST, while the room connection is still
    // live and before `joined` is taken — the camera slot lives in
    // `joined.media`, and the shared teardown needs the state to reach it.
    // leave_room can't see an in-flight camera (start awaits its first
    // frame), which is exactly why the generation check in
    // `camera_session::start_camera_publish_with_device` exists.
    crate::camera_session::stop_camera_publish(app, state).await;
    let joined = state.joined.lock_unpoisoned().take();
    let Some(mut joined) = joined else {
        return;
    };

    if let Some(cancellation) = joined.watcher_cancellation.take() {
        cancellation.cancel();
    }

    // Reverse ownership order: local publications, remote playout, then room.
    // (The camera was already taken by `stop_camera_publish` above.)
    let MediaResources {
        camera: _,
        microphone,
        playout,
        screen_audio_sources: _,
        mut screen_audio,
        shares,
    } = std::mem::take(&mut joined.media);
    for share in shares {
        let token = share.token;
        stop_share(app, share, joined.room_connection.clone()).await;
        emit_share_state_changed(app, token, false);
    }
    // Receiver-side: retire every remote compositor window (we are no longer
    // in a room; remote shares must not linger).
    crate::windows_compositor::remove_all(app).await;
    // Petal View selectors are meeting-scoped too: close them so no hollow
    // window outlives the session (each close stops its share + unregisters).
    crate::region_window::close_all_region_windows(app).await;
    let serial = state.lock_screen_audio_control().await;
    for (_, track) in std::mem::take(&mut screen_audio) {
        track.stop().await;
    }
    drop(serial);
    drop(microphone);
    drop(playout);

    if close_connection {
        if let Err(error) = joined.room_connection.room().close().await {
            log::warn!("windows session: error closing room during {reason}: {error}");
        }
    }

    let _ = tauri::Emitter::emit(
        app,
        "room-left",
        RoomLeftEvent {
            room_name: joined.room_record.name.clone(),
        },
    );
    log::info!(
        "windows session: left '{}' via {reason}",
        crate::logging::log_safe_quoted(&joined.room_record.name)
    );
    crate::analytics::meeting_left();
}

#[tauri::command]
pub fn current_room(state: tauri::State<'_, SessionState>) -> Option<String> {
    state.current_room_name()
}

#[tauri::command]
pub fn room_presence(
    state: tauri::State<'_, SessionState>,
) -> Vec<crate::presence::PresentParticipant> {
    state.presence_snapshot()
}

/// Mid-meeting display-name change from the Settings window. Renames the
/// LiveKit local participant (so every peer's roster follows via
/// `ParticipantNameChanged`) and rewrites this process's own roster entry,
/// which `join_room` seeds once and no RoomEvent ever refreshes. No-op when
/// not joined: the next `join_room` carries the new name itself. Empty names
/// fall back to the same "Guest" the frontend uses at join time.
#[tauri::command]
pub async fn set_display_name(
    app: tauri::AppHandle,
    state: tauri::State<'_, SessionState>,
    name: String,
) -> Result<(), String> {
    let Some((room, presence, room_name)) = state.joined_room_for_rename() else {
        return Ok(());
    };
    let name = name.trim();
    let display = if name.is_empty() { "Guest" } else { name }.to_string();
    room.local_participant()
        .set_name(display.clone())
        .await
        .map_err(|e| format!("set_name failed: {e}"))?;
    crate::presence::set_local_name(&app, &presence, &room_name, &display);
    Ok(())
}

#[tauri::command]
pub fn remote_control_allowed(state: tauri::State<'_, SessionState>) -> bool {
    state.remote_control_allowed()
}

#[tauri::command]
pub fn remote_control_policy(state: tauri::State<'_, SessionState>) -> RemoteControlPolicy {
    state.remote_control_policy()
}

#[tauri::command]
pub fn set_remote_control_policy(
    app: tauri::AppHandle,
    state: tauri::State<'_, SessionState>,
    policy: RemoteControlPolicy,
) -> RemoteControlPolicy {
    state.set_remote_control_policy(policy);
    if !policy.allows_requests() {
        crate::remote_control::revoke_all(&app);
    }
    policy
}

#[tauri::command]
pub fn set_remote_control_allowed(
    app: tauri::AppHandle,
    state: tauri::State<'_, SessionState>,
    allowed: bool,
) -> bool {
    state.set_remote_control_allowed(allowed);
    if !allowed {
        crate::remote_control::revoke_all(&app);
    }
    allowed
}

/// Whether remote peers may control one shared window. This is independent of
/// the meeting-wide policy; both gates must allow control.
#[tauri::command]
pub fn share_remote_control_allowed(state: tauri::State<'_, SessionState>, window_id: u32) -> bool {
    state.share_allows_remote_control(window_id)
}

/// Set the host-side remote-control lock for one shared window and publish the
/// discoverability hint after authorization has been updated.
#[tauri::command]
pub async fn set_share_remote_control_allowed(
    app: tauri::AppHandle,
    state: tauri::State<'_, SessionState>,
    window_id: u32,
    allowed: bool,
) -> Result<bool, String> {
    let Some(previous) = state.set_share_allows_remote_control(window_id, allowed) else {
        return Ok(false);
    };
    if previous == allowed {
        return Ok(allowed);
    }
    if !allowed {
        crate::remote_control::revoke_window(&app, window_id, "share-remote-control-locked");
    }
    if let Some(connection) = state.room_connection() {
        connection
            .set_shared_remote_control_allowed(window_id, allowed)
            .await;
    }
    log::info!(
        "windows session: share {window_id} remote control {} by the sharer",
        if allowed { "ALLOWED" } else { "LOCKED" }
    );
    Ok(allowed)
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct RoomLeftEvent {
    room_name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn room(id: &str, name: &str) -> crate::rooms::RoomRecord {
        crate::rooms::RoomRecord {
            id: id.to_string(),
            name: name.to_string(),
            access_code: None,
            display_name: None,
            slug: name.to_string(),
            created_at_ms: 1,
            last_joined_ms: Some(1),
            open: true,
        }
    }

    #[test]
    fn display_shares_force_full_control_but_windows_keep_requested_mode() {
        assert_eq!(
            effective_control_mode(
                TargetKind::Display,
                crate::remote_control_core::RemoteControlMode::CursorPreserving
            ),
            crate::remote_control_core::RemoteControlMode::FullControl
        );
        assert_eq!(
            effective_control_mode(
                TargetKind::Window,
                crate::remote_control_core::RemoteControlMode::CursorPreserving
            ),
            crate::remote_control_core::RemoteControlMode::CursorPreserving
        );
    }

    #[test]
    fn windows_session_starts_outside_a_room_with_empty_native_state() {
        let state = SessionState::default();
        assert_eq!(state.current_room_name(), None);
        assert!(state.presence_snapshot().is_empty());
        assert!(!state.camera_publishing());
        assert!(!state.camera_intent());
    }

    #[test]
    fn per_share_remote_control_fails_closed_without_a_live_share() {
        let state = SessionState::default();
        assert!(!state.share_allows_remote_control(4242));
        assert_eq!(state.set_share_allows_remote_control(4242, true), None);
    }

    #[test]
    fn ai_chat_adapters_report_no_room_and_no_shares_when_unjoined() {
        // The AI chat commands gate on is_share_active + current_room_record;
        // with no joined session both must answer "no" — never panic, never
        // pretend a share exists.
        let state = SessionState::default();
        assert!(!state.is_share_active(1));
        assert!(!state.is_share_active(0));
        assert!(state.current_room_record().is_none());
    }

    #[test]
    fn unavailable_microphone_rejects_unmute_without_changing_truth() {
        let state = SessionState::default();
        assert!(state.mic_muted());

        let error = state
            .set_mic_muted(false)
            .expect_err("unmute must fail without a retained microphone");

        assert_eq!(error, "microphone unavailable");
        assert!(state.mic_muted());
    }

    #[tokio::test]
    async fn mic_control_transactions_serialize() {
        let state = SessionState::default();
        let first = state.lock_mic_control().await;

        assert!(state.mic_control_lock.try_lock().is_err());
        drop(first);
        assert!(state.mic_control_lock.try_lock().is_ok());
    }

    #[tokio::test]
    async fn camera_control_transactions_serialize() {
        let state = SessionState::default();
        let first = state.camera_control_lock.lock().await;

        assert!(state.camera_control_lock.try_lock().is_err());
        drop(first);
        assert!(state.camera_control_lock.try_lock().is_ok());
    }

    #[test]
    fn camera_intent_round_trips_without_a_room() {
        let state = SessionState::default();
        assert!(!state.camera_intent());

        state.set_camera_intent(true);
        assert!(state.camera_intent());

        state.set_camera_intent(false);
        assert!(!state.camera_intent());
    }

    #[test]
    fn windows_media_resources_start_without_native_handles() {
        let media = MediaResources::default();

        assert!(media.microphone.is_none());
        assert!(media.playout.is_none());
        assert!(media.camera.is_none());
        assert!(media.shares.is_empty());
    }

    /// Real Windows integration gate. Unlike the headless unit matrix, this
    /// enters the production `start_share_token`/`stop_share_token` path,
    /// starts WGC for an operator-supplied HWND, publishes through LiveKit,
    /// starts the native screen-audio adapter, and then verifies terminal
    /// cleanup. It is ignored in ordinary CI because it needs a visible target,
    /// Windows capture/audio consent, and a LiveKit server.
    #[cfg(target_os = "windows")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires PETAL_WINDOWS_MEDIA_INTEGRATION, a visible HWND, and LiveKit"]
    async fn windows_share_session_runs_real_wgc_livekit_audio_lifecycle() {
        use crate::remote_control_core::RemoteControlMode;
        use crate::screen_audio::{AudioSourceKey, ScreenAudioCapture};
        use crate::transport::publisher::SharedSourceKind;
        use crate::transport::RoomConnection;
        use futures::StreamExt;
        use livekit::prelude::{RemoteTrack, RoomEvent};
        use livekit::track::VideoQuality;
        use livekit::webrtc::video_stream::native::NativeVideoStream;
        use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
        use std::time::Duration;
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

        assert_eq!(
            std::env::var("PETAL_WINDOWS_MEDIA_INTEGRATION").as_deref(),
            Ok("1"),
            "set PETAL_WINDOWS_MEDIA_INTEGRATION=1 to run the real Windows gate"
        );
        let raw_handle = std::env::var("PETAL_WINDOWS_CAPTURE_HWND")
            .expect("PETAL_WINDOWS_CAPTURE_HWND must identify a visible target")
            .parse::<usize>()
            .expect("PETAL_WINDOWS_CAPTURE_HWND must be a decimal HWND");
        let hwnd = HWND(raw_handle as *mut std::ffi::c_void);
        let mut owner_pid = 0_u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut owner_pid)) };
        assert_ne!(
            owner_pid, 0,
            "the selected HWND must have a live owner process"
        );
        let token = crate::windows_capture_target::register(raw_handle, owner_pid)
            .expect("register the operator-selected HWND");

        let url = crate::transport::token::livekit_url()
            .expect("LIVEKIT_URL must point at a running LiveKit server");
        let room_name = format!(
            "petal-windows-media-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_millis()
        );
        let identity = format!("windows-media-{}", std::process::id());
        let access = crate::transport::mint_access_token(&identity, &room_name, true, true)
            .expect("mint an integration token");
        let connection = Arc::new(
            RoomConnection::connect(&url, &access)
                .await
                .expect("connect the production RoomConnection"),
        );
        let observer_identity = format!("windows-media-observer-{}", std::process::id());
        let observer_access =
            crate::transport::mint_access_token(&observer_identity, &room_name, false, true)
                .expect("mint the observer token");
        let observer = RoomConnection::connect(&url, &observer_access)
            .await
            .expect("connect the real LiveKit observer");
        let mut observer_events = observer
            .take_compositor_events()
            .expect("observer connect-time event receiver");
        let observer_stop = Arc::new(AtomicBool::new(false));
        let observer_frames = Arc::new(AtomicU64::new(0));
        let observer_requested_low = Arc::new(AtomicBool::new(false));
        let observer_min_width = Arc::new(AtomicU32::new(u32::MAX));
        let stop_observer = observer_stop.clone();
        let count_observer_frames = observer_frames.clone();
        let request_observer_low = observer_requested_low.clone();
        let record_observer_width = observer_min_width.clone();
        let observer_task = tokio::spawn(async move {
            while let Some(event) = observer_events.recv().await {
                if stop_observer.load(Ordering::Acquire) {
                    break;
                }
                let RoomEvent::TrackSubscribed {
                    track, publication, ..
                } = event
                else {
                    continue;
                };
                let RemoteTrack::Video(video) = track else {
                    continue;
                };
                if crate::transport::publisher::window_id_from_track_name(&video.name())
                    != Some(token)
                {
                    continue;
                }
                // This observer is the deliberately weak leg. Make the
                // receiver-local request explicit, then verify that the
                // decoded frames actually move to the lower simulcast rung.
                publication.set_video_quality(VideoQuality::Low);
                request_observer_low.store(true, Ordering::Release);
                let mut stream = NativeVideoStream::new(video.rtc_track());
                while let Some(frame) = stream.next().await {
                    if stop_observer.load(Ordering::Acquire) {
                        return;
                    }
                    count_observer_frames.fetch_add(1, Ordering::Relaxed);
                    record_observer_width.fetch_min(frame.buffer.width(), Ordering::Relaxed);
                }
            }
        });
        let capable_identity = format!("windows-media-capable-{}", std::process::id());
        let capable_access =
            crate::transport::mint_access_token(&capable_identity, &room_name, false, true)
                .expect("mint the capable observer token");
        let capable_observer = RoomConnection::connect(&url, &capable_access)
            .await
            .expect("connect the real capable LiveKit observer");
        let mut capable_events = capable_observer
            .take_compositor_events()
            .expect("capable observer connect-time event receiver");
        let capable_subscriptions = Arc::new(AtomicU64::new(0));
        let capable_max_width = Arc::new(AtomicU32::new(0));
        let capable_replacement_frames = Arc::new(AtomicU64::new(0));
        let capable_count = capable_subscriptions.clone();
        let count_replacement_frames = capable_replacement_frames.clone();
        let record_capable_width = capable_max_width.clone();
        let capable_task = tokio::spawn(async move {
            let mut stream_tasks = tokio::task::JoinSet::new();
            while let Some(event) = capable_events.recv().await {
                let RoomEvent::TrackSubscribed {
                    track, publication, ..
                } = event
                else {
                    continue;
                };
                let RemoteTrack::Video(video) = track else {
                    continue;
                };
                if crate::transport::publisher::window_id_from_track_name(&video.name())
                    != Some(token)
                {
                    continue;
                }
                // Keep this observer explicitly HIGH. Its decoded dimensions
                // are the runtime oracle that the weak observer's LOW request
                // did not mutate the sender or this independent subscriber.
                publication.set_video_quality(VideoQuality::High);
                let subscription_number = capable_count.fetch_add(1, Ordering::AcqRel) + 1;
                let record_width = record_capable_width.clone();
                let count_replacement = count_replacement_frames.clone();
                stream_tasks.spawn(async move {
                    let mut stream = NativeVideoStream::new(video.rtc_track());
                    while let Some(frame) = stream.next().await {
                        record_width.fetch_max(frame.buffer.width(), Ordering::Relaxed);
                        if subscription_number > 1 {
                            count_replacement.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });
            }
            stream_tasks.abort_all();
            while stream_tasks.join_next().await.is_some() {}
        });

        let app = tauri::Builder::default()
            .build(tauri::generate_context!())
            .expect("build the Tauri app for the integration gate");
        app.manage(SessionState::default());
        let state = app.state::<SessionState>();
        let generation = state.begin_room_generation();
        *state.joined.lock_unpoisoned() = Some(WindowsMediaSession {
            room_record: room("windows-media-integration", &room_name),
            identity,
            room_connection: connection.clone(),
            presence: Arc::new(crate::presence::PresenceState::default()),
            media: MediaResources::default(),
            watcher_cancellation: None,
        });
        assert!(generation.is_current());

        // Exercise the actual session coordinator: WGC first, then the
        // publisher and best-effort process-loopback audio.
        start_share_token(
            app.handle().clone(),
            &state,
            token,
            RemoteControlMode::CursorPreserving,
            "#ffffff".to_string(),
        )
        .await
        .expect("production Windows share start");
        assert!(state.is_share_active(token));
        let window_audio_source =
            AudioSourceKey::for_share(SharedSourceKind::Window, Some(owner_pid))
                .expect("a registered HWND must have a process audio source");
        assert!(
            state.screen_audio_handle_present(window_audio_source),
            "production share start did not publish the native process-loopback audio track"
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while observer_frames.load(Ordering::Acquire) == 0 && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            observer_frames.load(Ordering::Acquire) > 0,
            "real WGC -> production publisher -> LiveKit -> observer decoded no frames"
        );
        let capable_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while capable_subscriptions.load(Ordering::Acquire) == 0
            && tokio::time::Instant::now() < capable_deadline
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            capable_subscriptions.load(Ordering::Acquire) > 0,
            "the independent capable LiveKit observer never subscribed"
        );

        // A LOW request must be observable in decoded output, not merely in
        // the test observer's call site. Wait for both legs to produce frames
        // and compare the actual received dimensions.
        let quality_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while (observer_min_width.load(Ordering::Acquire) == u32::MAX
            || capable_max_width.load(Ordering::Acquire) == 0
            || observer_min_width.load(Ordering::Acquire)
                >= capable_max_width.load(Ordering::Acquire))
            && tokio::time::Instant::now() < quality_deadline
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            observer_requested_low.load(Ordering::Acquire),
            "weak observer never issued its receiver-local LOW request"
        );
        assert!(
            observer_min_width.load(Ordering::Acquire) < capable_max_width.load(Ordering::Acquire),
            "weak observer did not receive a lower decoded simulcast rung: weak_min_width={} capable_max_width={}",
            observer_min_width.load(Ordering::Acquire),
            capable_max_width.load(Ordering::Acquire)
        );
        let initial_capable_subscriptions = capable_subscriptions.load(Ordering::Acquire);

        // Republish the same production window name while the old publication
        // is still live. The existing session pump reads this shared slot, so
        // the replacement receives real WGC frames before the old SID is
        // removed (the delayed-unpublish/republish contract).
        let (old_published, width, height) = {
            let joined = state.joined.lock_unpoisoned();
            let share = &joined
                .as_ref()
                .expect("joined integration session")
                .media
                .shares[0];
            let old = share.shared.published.lock_unpoisoned().clone();
            let width = old.width();
            let height = old.height();
            (old, width, height)
        };
        let replacement = Arc::new(
            connection
                .publish_window_at(
                    width,
                    height,
                    crate::transport::publisher::ShareQuality::Full,
                    Some(token),
                )
                .await
                .expect("publish the real replacement window track"),
        );
        {
            let joined = state.joined.lock_unpoisoned();
            let share = &joined
                .as_ref()
                .expect("joined integration session")
                .media
                .shares[0];
            *share.shared.published.lock_unpoisoned() = replacement;
        }
        old_published
            .unpublish()
            .await
            .expect("unpublish the superseded real window track");

        let republish_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while (capable_subscriptions.load(Ordering::Acquire) <= initial_capable_subscriptions
            || capable_replacement_frames.load(Ordering::Acquire) == 0)
            && tokio::time::Instant::now() < republish_deadline
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            capable_subscriptions.load(Ordering::Acquire) > initial_capable_subscriptions,
            "capable observer did not receive a TrackSubscribed event for the replacement publication"
        );
        assert!(
            capable_replacement_frames.load(Ordering::Acquire) > 0,
            "capable observer subscribed to the replacement but decoded no replacement frames"
        );

        // Reconnect a receiver while the replacement is active and require a
        // connect-time TrackSubscribed event for the existing publication.
        observer_stop.store(true, Ordering::Release);
        observer.room().close().await.ok();
        tokio::time::timeout(Duration::from_secs(2), observer_task)
            .await
            .expect("weak observer task must terminate on reconnect")
            .expect("weak observer task must not panic");
        capable_observer.room().close().await.ok();
        tokio::time::timeout(Duration::from_secs(2), capable_task)
            .await
            .expect("capable observer task must terminate")
            .expect("capable observer task must not panic");
        let reconnected = RoomConnection::connect(&url, &capable_access)
            .await
            .expect("reconnect the capable LiveKit observer");
        let mut reconnected_events = reconnected
            .take_compositor_events()
            .expect("reconnected observer event receiver");
        let reconnected_frame = tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(event) = reconnected_events.recv().await {
                let RoomEvent::TrackSubscribed { track, .. } = event else {
                    continue;
                };
                if let RemoteTrack::Video(video) = track {
                    if crate::transport::publisher::window_id_from_track_name(&video.name())
                        == Some(token)
                    {
                        let mut stream = NativeVideoStream::new(video.rtc_track());
                        return stream.next().await.is_some();
                    }
                }
            }
            false
        })
        .await
        .expect("reconnected observer subscription deadline");
        assert!(
            reconnected_frame,
            "reconnect did not resubscribe and decode the replacement"
        );
        reconnected.room().close().await.ok();

        // Normal stop is the first real terminal path.
        stop_share_token(&app.handle(), &state, token)
            .await
            .expect("production Windows share stop");
        assert!(!state.is_share_active(token));
        assert!(state.shared_window_ids().is_empty());

        // Re-start the same real share, stop only WGC, and verify that a
        // capture crash does not pretend the LiveKit publication disappeared;
        // the production stop path must still clean up the missed-unpublish
        // tail.
        start_share_token(
            app.handle().clone(),
            &state,
            token,
            RemoteControlMode::CursorPreserving,
            "#ffffff".to_string(),
        )
        .await
        .expect("production Windows share restart");
        state
            .joined
            .lock_unpoisoned()
            .as_ref()
            .expect("joined integration session")
            .media
            .shares
            .first()
            .expect("restarted share")
            .capture
            .request_stop_for_test();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            state.is_share_active(token),
            "capture failure must leave the publication for bounded recovery"
        );
        stop_share_token(&app.handle(), &state, token)
            .await
            .expect("cleanup after missed unpublish");

        // Exercise delayed unpublish on a fresh production share. The fault
        // injection delays only the SDK tail; visual/native cleanup still
        // returns through the normal session coordinator.
        start_share_token(
            app.handle().clone(),
            &state,
            token,
            RemoteControlMode::CursorPreserving,
            "#ffffff".to_string(),
        )
        .await
        .expect("production Windows share delayed-stop setup");
        std::env::set_var("PETAL_TEST_UNPUBLISH_DELAY_MS", "100");
        let delayed_start = std::time::Instant::now();
        stop_share_token(&app.handle(), &state, token)
            .await
            .expect("delayed production Windows share stop");
        std::env::remove_var("PETAL_TEST_UNPUBLISH_DELAY_MS");
        assert!(delayed_start.elapsed() >= Duration::from_millis(100));

        // Exercise the adapter independently as well: a visual stop must not
        // await this signaling path, and repeated stop is idempotent.
        let system_audio = ScreenAudioCapture::start(AudioSourceKey::SystemOutput, |_| {})
            .expect("Windows loopback audio must start for this integration gate");
        system_audio.stop().expect("first audio stop");
        system_audio.stop().expect("repeated audio stop");

        state.invalidate_room_generation();
        observer_stop.store(true, Ordering::Release);
        observer.room().close().await.ok();
        connection.room().close().await.ok();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    #[test]
    fn share_error_kinds_match_the_frontend_toast_map() {
        // `shareErrors.ts`/`shareErrorDisplay` decode these exact kind
        // strings; new kinds would render as the generic toast.
        let cases = [
            (ShareError::NotInRoom, "notInRoom"),
            (
                ShareError::WindowNotFound("7".to_string()),
                "windowNotFound",
            ),
            (ShareError::Capture("boom".to_string()), "capture"),
            (ShareError::TooManyShares, "tooManyShares"),
            (ShareError::Unknown("x".to_string()), "unknown"),
        ];
        for (error, expected_kind) in cases {
            let value = serde_json::to_value(error).unwrap();
            assert_eq!(value["kind"], expected_kind);
        }
    }

    #[test]
    fn share_constants_pin_the_receiver_compositor_contract() {
        assert_eq!(MAX_CONCURRENT_SHARES, 4);
        assert_eq!(SHARE_FIRST_FRAME_TIMEOUT.as_secs(), 5);
    }

    #[test]
    fn unset_or_empty_audio_opt_out_keeps_wasapi_enabled() {
        assert!(!audio_is_disabled(None));
        assert!(!audio_is_disabled(Some("")));
    }

    #[test]
    fn truthy_audio_opt_out_disables_wasapi_but_false_like_values_keep_it() {
        // Same contract as transport/audio.rs (b05e36cb, #812): "0"/"false"/
        // "no"/"off" keep audio ENABLED. This stub test still asserted the
        // pre-#812 "any non-empty value disables" rule and was red on every
        // Windows CI run since (#912).
        assert!(audio_is_disabled(Some("1")));
        assert!(audio_is_disabled(Some("true")));
        assert!(!audio_is_disabled(Some("false")));
        assert!(!audio_is_disabled(Some("0")));
    }

    #[tokio::test]
    async fn media_watcher_cancellation_is_observed_without_a_room_event() {
        let (cancellation, mut cancelled) = media_watcher_cancellation();

        cancellation.cancel();
        cancelled
            .changed()
            .await
            .expect("retained media session still owns the cancellation sender");
        assert!(*cancelled.borrow());
    }

    #[test]
    fn room_generations_invalidate_stale_watchers() {
        let state = SessionState::default();
        let first = state.begin_room_generation();
        assert!(first.is_current());

        let second = state.begin_room_generation();
        assert!(!first.is_current());
        assert!(second.is_current());

        assert!(!first.invalidate_if_current());
        assert!(second.is_current());
        assert!(second.invalidate_if_current());
        assert!(!second.is_current());
    }

    #[test]
    fn durable_room_identity_makes_same_room_rejoin_idempotent() {
        let current = room("stable-room-id", "old-display-slug");
        let same_room = room("stable-room-id", "new-display-slug");
        let other_room = room("other-room-id", "old-display-slug");

        assert!(is_same_room(&current, &same_room));
        assert!(!is_same_room(&current, &other_room));
    }

    #[tokio::test]
    async fn transition_lock_serializes_join_switch_and_leave() {
        let state = SessionState::default();
        let first_transition = state.transition_lock.lock().await;
        assert!(
            state.transition_lock.try_lock().is_err(),
            "a second room transition must not run concurrently"
        );
        drop(first_transition);
        assert!(state.transition_lock.try_lock().is_ok());
    }

    #[test]
    fn telepointer_frame_insets_invisible_dwm_resize_borders() {
        use crate::platform::cg::WindowFrame;
        // Captioned window (GitHub Desktop, observed live): the client rect
        // includes the invisible resize borders, so the capture = client
        // minus them: 1484x858 -> item 1470x851, origin = client.x + 7.
        let client = WindowFrame {
            x: 46,
            y: 172,
            width: 1484,
            height: 858,
        };
        let outer = WindowFrame {
            x: 38,
            y: 140,
            width: 1500,
            height: 898,
        };
        let content = telepointer_content_frame(client, outer, Some((1470, 851)));
        assert_eq!(content.x, 46 + (1484 - 1470) / 2);
        assert_eq!(content.y, 172);
        assert_eq!(content.width, 1470);
        assert_eq!(content.height, 851);

        // Borderless window (RUN, observed live): GetClientRect is the pure
        // drawing area; the capture = the OUTER rect minus the same invisible
        // borders: 1722x977 -> item 1708x970, origin = outer.x + 7, top = 0.
        let run_client = WindowFrame {
            x: 94,
            y: 41,
            width: 1706,
            height: 969,
        };
        let run_outer = WindowFrame {
            x: 86,
            y: 41,
            width: 1722,
            height: 977,
        };
        let run = telepointer_content_frame(run_client, run_outer, Some((1708, 970)));
        assert_eq!(run.x, 86 + (1722 - 1708) / 2);
        assert_eq!(run.y, 41);
        assert_eq!(run.width, 1708);
        assert_eq!(run.height, 970);

        // Maximized windows have no invisible borders: the item size matches
        // the client rect, so the conversion must no-op (exact cursor).
        let maximized = WindowFrame {
            x: 0,
            y: 0,
            width: 1536,
            height: 816,
        };
        assert_eq!(
            telepointer_content_frame(maximized, maximized, Some((1536, 816))),
            maximized
        );

        // Captioned window whose capture is LARGER than the client (the RUN
        // window, observed live): the WGC item includes the visible border
        // and covers the full GetWindowRect region — the content must be the
        // item anchored at the OUTER origin, not the raw client (client
        // 1706x969 vs item 1708x970 was the residual ~1-2px tag offset).
        let run_client = WindowFrame {
            x: 94,
            y: 41,
            width: 1706,
            height: 969,
        };
        let run_outer = WindowFrame {
            x: 92,
            y: 39,
            width: 1708,
            height: 970,
        };
        let run = telepointer_content_frame(run_client, run_outer, Some((1708, 970)));
        assert_eq!(
            run,
            WindowFrame {
                x: 92,
                y: 39,
                width: 1708,
                height: 970
            }
        );

        // Unknown capture size (no frame pumped yet): fall back to the
        // client rect rather than dropping the share from the snapshot.
        assert_eq!(telepointer_content_frame(client, outer, None), client);
    }
}
