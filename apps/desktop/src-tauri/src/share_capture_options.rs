//! Ephemeral per-window capture choices for Windows shares: the frame-rate
//! ceiling for the window's next share, and whether the system cursor is
//! captured into its frames.
//!
//! Unlike `share_priority`, these are deliberately not persisted. They belong to
//! the next share of that window and are cleared when the share ends or fails to
//! start. The publisher stays the authority for the *effective* cadence once a
//! share is active: the choice is a ceiling, capped again by the published
//! geometry's Level 5.2 limit.
//!
//! Two surfaces write the same per-token store: the hover tab by window id, and
//! Petal View by selector label, which resolves to the token the share will
//! publish under.

use crate::platform::cg::WindowFrame;
use crate::sync_ext::MutexExt;
use crate::transport::publisher::share_fps_ceiling_for_geometry;
use crate::windows_capture_target::{self, TargetKind};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};

pub const DEFAULT_SHARE_FPS: u32 = 30;

/// Offered ceilings, coarsest first, so the menu reads as a scale.
///
/// `15` is the poor-network / low-power entry: at every geometry it doubles the
/// bits each encoded frame gets at a fixed target, which is the right trade for
/// text-heavy screen content. `120` is deliberately not offered even where the
/// geometry affords it: the capture path is refresh-bound and the product
/// target is "4K at most 60".
pub const SHARE_FPS_CHOICES: [u32; 3] = [15, 30, 60];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareFpsChoice {
    pub fps: u32,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareCaptureOptions {
    /// Echoed so a caller can match a response to the window it asked about,
    /// mirroring `ShareAudioState`'s contract.
    pub window_id: u32,
    pub choices: Vec<ShareFpsChoice>,
    pub selected_fps: u32,
    /// Whether the system cursor is captured into the video. Off by default,
    /// matching macOS (`with_shows_cursor(false)`): the sharer's pointer
    /// already travels on the telepointer channel.
    pub cursor_in_video: bool,
}

static PENDING_FPS: LazyLock<Mutex<HashMap<u32, u32>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Windows whose next share captures the system cursor. An absent entry is the
/// default (off), so this only ever holds opt-ins.
static CURSOR_IN_VIDEO: LazyLock<Mutex<HashSet<u32>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Clears a pre-share choice unless the share-start transaction reaches its
/// successful commit. This covers every early-return/error path without
/// requiring each capture teardown branch to remember the store separately.
pub(crate) struct ClearOnFailure {
    window_id: u32,
    keep: bool,
}

impl ClearOnFailure {
    pub(crate) fn keep(mut self) {
        self.keep = true;
    }
}

impl Drop for ClearOnFailure {
    fn drop(&mut self) {
        if !self.keep {
            clear(self.window_id);
        }
    }
}

pub(crate) fn clear_on_failure(window_id: u32) -> ClearOnFailure {
    ClearOnFailure {
        window_id,
        keep: false,
    }
}

/// Every offered ceiling, each enabled only when the geometry's Level 5.2
/// ceiling affords it: 60 stays available at 4K and is disabled at 5K-class.
pub fn effective_choices(width: u32, height: u32) -> Vec<ShareFpsChoice> {
    let ceiling = share_fps_ceiling_for_geometry(width, height).unwrap_or(DEFAULT_SHARE_FPS);
    SHARE_FPS_CHOICES
        .iter()
        .copied()
        .map(|fps| ShareFpsChoice {
            fps,
            enabled: fps <= ceiling,
        })
        .collect()
}

fn is_supported_fps(fps: u32) -> bool {
    SHARE_FPS_CHOICES.contains(&fps)
}

/// The frame-rate ceiling this window's next share publishes with.
pub(crate) fn pending(window_id: u32) -> u32 {
    PENDING_FPS
        .lock_unpoisoned()
        .get(&window_id)
        .copied()
        .unwrap_or(DEFAULT_SHARE_FPS)
}

fn set_pending(window_id: u32, fps: u32) -> Result<(), String> {
    if !is_supported_fps(fps) {
        return Err(format!(
            "unsupported screen-share frame rate {fps}; expected one of {:?}",
            SHARE_FPS_CHOICES
        ));
    }
    PENDING_FPS.lock_unpoisoned().insert(window_id, fps);
    Ok(())
}

pub(crate) fn clear(window_id: u32) {
    PENDING_FPS.lock_unpoisoned().remove(&window_id);
    CURSOR_IN_VIDEO.lock_unpoisoned().remove(&window_id);
}

/// Whether this window's next share captures the system cursor. Read by the
/// capture session before `StartCapture`.
pub(crate) fn cursor_in_video(window_id: u32) -> bool {
    CURSOR_IN_VIDEO.lock_unpoisoned().contains(&window_id)
}

fn set_cursor_in_video(window_id: u32, enabled: bool) {
    let mut opted_in = CURSOR_IN_VIDEO.lock_unpoisoned();
    if enabled {
        opted_in.insert(window_id);
    } else {
        opted_in.remove(&window_id);
    }
}

fn target_frame(window_id: u32) -> Result<WindowFrame, String> {
    let target = windows_capture_target::resolve(window_id)
        .map_err(|error| format!("cannot resolve share target {window_id}: {error}"))?;
    let frame = match target.kind() {
        TargetKind::Window => crate::platform::windows::window_frame_for_raw(target.raw_handle()),
        TargetKind::Display => crate::platform::windows::display_frame_for_raw(target.raw_handle()),
    }
    .ok_or_else(|| format!("cannot resolve physical frame for share target {window_id}"))?;
    if frame.width <= 0 || frame.height <= 0 {
        return Err(format!(
            "share target {window_id} has invalid frame {}x{}",
            frame.width, frame.height
        ));
    }
    Ok(frame)
}

fn options_for_geometry(
    window_id: u32,
    width: u32,
    height: u32,
    selected_fps: u32,
    shared: bool,
) -> ShareCaptureOptions {
    let feasible = effective_choices(width, height);
    // A live share's choices cannot be changed -- the cadence is fixed at
    // publish -- so every entry is present but inert while it runs. The
    // cadence shown is the one the encoder is running at, which may be lower
    // than the choice that produced it.
    let selected_fps = if feasible
        .iter()
        .any(|choice| choice.fps == selected_fps && choice.enabled)
    {
        selected_fps
    } else {
        DEFAULT_SHARE_FPS
    };
    let choices = feasible
        .into_iter()
        .map(|choice| ShareFpsChoice {
            enabled: choice.enabled && !shared,
            ..choice
        })
        .collect();
    ShareCaptureOptions {
        window_id,
        choices,
        selected_fps,
        cursor_in_video: cursor_in_video(window_id),
    }
}

/// The geometry a window's frame-rate feasibility is judged on BEFORE its share
/// starts.
///
/// Petal View's answer is the geometry the region capture will publish: the
/// selector's own dimensions at the owning display's resolution, made even by
/// truncation (`ClippedPhysicalRegion::output_*`). The monitor's own dimensions
/// would be wrong -- they would judge a 720p ROI by 5120x2880 and disable 60 fps
/// for every ROI on a 5K display. That helper is only available once a capture
/// has latched the selector's display; until then, and for a selector outside
/// its display, fall back to the target's physical frame, which for a region
/// token IS the selector's own rect.
///
/// Either way the publish path re-caps the chosen cadence to the geometry it
/// really publishes, so a wrong estimate can mislead this menu and cannot produce
/// a non-conformant stream.
fn unshared_geometry(window_id: u32) -> Result<(u32, u32), String> {
    if let Some(physical) = crate::region_window::resolve(window_id).and_then(|source| {
        source
            .display
            .and_then(|display| display.clipped_physical_roi(source.frame))
    }) {
        return Ok((physical.output_width, physical.output_height));
    }
    let frame = target_frame(window_id)?;
    Ok((frame.width as u32, frame.height as u32))
}

#[tauri::command]
pub fn share_capture_options(
    state: tauri::State<'_, crate::session::SessionState>,
    window_id: u32,
) -> Result<ShareCaptureOptions, String> {
    share_capture_options_for_state(&state, window_id)
}

/// The hover tab's command body, also reachable label-addressed from Petal View
/// (`region_window::region_share_capture_options`) so both surfaces answer from
/// this one rule instead of two that could disagree about what is feasible.
pub(crate) fn share_capture_options_for_state(
    state: &crate::session::SessionState,
    window_id: u32,
) -> Result<ShareCaptureOptions, String> {
    // While shared, the live published geometry and cadence are the authority:
    // they are what the encoder was configured with.
    if let Some((width, height, cadence)) = state.share_published_cadence(window_id) {
        return Ok(options_for_geometry(
            window_id, width, height, cadence, true,
        ));
    }
    let (width, height) = unshared_geometry(window_id)?;
    Ok(options_for_geometry(
        window_id,
        width,
        height,
        pending(window_id),
        false,
    ))
}

#[tauri::command]
pub fn set_share_fps(
    state: tauri::State<'_, crate::session::SessionState>,
    window_id: u32,
    fps: u32,
) -> Result<ShareCaptureOptions, String> {
    set_share_fps_for_state(&state, window_id, fps)
}

/// [`set_share_fps`]'s command body; see [`share_capture_options_for_state`].
/// Refused while the window is shared: the cadence is fixed at publish, so the
/// choice applies to the next share.
pub(crate) fn set_share_fps_for_state(
    state: &crate::session::SessionState,
    window_id: u32,
    fps: u32,
) -> Result<ShareCaptureOptions, String> {
    if state.is_share_active(window_id) {
        return Err(format!(
            "cannot change the screen-share frame rate while window {window_id} is shared"
        ));
    }
    let (width, height) = unshared_geometry(window_id)?;
    let options = options_for_geometry(window_id, width, height, pending(window_id), false);
    let choice = options
        .choices
        .iter()
        .find(|choice| choice.fps == fps)
        .ok_or_else(|| format!("unsupported screen-share frame rate {fps}"))?;
    if !choice.enabled {
        return Err(format!(
            "screen-share frame rate {fps} is not feasible for {width}x{height}"
        ));
    }
    set_pending(window_id, fps)?;
    Ok(options_for_geometry(window_id, width, height, fps, false))
}

#[tauri::command]
pub fn set_share_cursor_in_video(
    state: tauri::State<'_, crate::session::SessionState>,
    window_id: u32,
    enabled: bool,
) -> Result<ShareCaptureOptions, String> {
    set_share_cursor_in_video_for_state(&state, window_id, enabled)
}

/// [`set_share_cursor_in_video`]'s command body; see
/// [`share_capture_options_for_state`]. Refused while the window is shared:
/// cursor capture is a capture-session property set before `StartCapture`, so
/// the choice applies to the next share.
pub(crate) fn set_share_cursor_in_video_for_state(
    state: &crate::session::SessionState,
    window_id: u32,
    enabled: bool,
) -> Result<ShareCaptureOptions, String> {
    if state.is_share_active(window_id) {
        return Err(format!(
            "cannot change cursor capture while window {window_id} is shared; it applies to the next share"
        ));
    }
    set_cursor_in_video(window_id, enabled);
    share_capture_options_for_state(state, window_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(fps, enabled)` pairs, so a three-entry list stays readable.
    fn choices_of(width: u32, height: u32) -> Vec<(u32, bool)> {
        effective_choices(width, height)
            .into_iter()
            .map(|choice| (choice.fps, choice.enabled))
            .collect()
    }

    #[test]
    fn choices_follow_the_geometry_ceiling() {
        let every = vec![(15, true), (30, true), (60, true)];
        assert_eq!(choices_of(1920, 1080), every);
        assert_eq!(choices_of(2560, 1440), every);
        // 4K is exactly the largest size that still affords 60.
        assert_eq!(choices_of(3840, 2160), every);
        // 5K-class: 60 would need H.264 Level 6.x, so it is listed but inert.
        let no_sixty = vec![(15, true), (30, true), (60, false)];
        assert_eq!(choices_of(4096, 2304), no_sixty);
        assert_eq!(choices_of(5120, 2880), no_sixty);
    }

    #[test]
    fn pending_values_are_per_window_and_clearable() {
        clear(7001);
        clear(7002);
        assert_eq!(pending(7001), DEFAULT_SHARE_FPS);
        assert_eq!(pending(7002), DEFAULT_SHARE_FPS);
        set_pending(7001, 60).expect("60 is a supported choice");
        assert_eq!(pending(7001), 60);
        assert_eq!(pending(7002), DEFAULT_SHARE_FPS);
        clear(7001);
        assert_eq!(pending(7001), DEFAULT_SHARE_FPS);
    }

    #[test]
    fn unsupported_pending_values_are_rejected() {
        clear(7003);
        assert!(set_pending(7003, 120).is_err());
        assert!(set_pending(7003, 0).is_err());
        assert_eq!(pending(7003), DEFAULT_SHARE_FPS);
    }

    #[test]
    fn failed_start_guard_clears_but_success_keeps_the_choice() {
        clear(7004);
        set_pending(7004, 60).expect("60 is a supported choice");
        {
            let _guard = clear_on_failure(7004);
        }
        assert_eq!(pending(7004), DEFAULT_SHARE_FPS);

        set_pending(7004, 60).expect("60 is a supported choice");
        {
            let guard = clear_on_failure(7004);
            guard.keep();
        }
        assert_eq!(pending(7004), 60);
        clear(7004);
    }

    #[test]
    fn cursor_capture_is_off_unless_opted_in_and_cleared_with_the_share() {
        clear(7060);
        assert!(!cursor_in_video(7060), "the default is off");
        set_cursor_in_video(7060, true);
        assert!(cursor_in_video(7060));
        // Per-window, like the frame rate.
        clear(7061);
        assert!(!cursor_in_video(7061));
        // Turning it back off removes the opt-in.
        set_cursor_in_video(7060, false);
        assert!(!cursor_in_video(7060));
        // Unshare (or a failed start) clears it with the frame rate.
        set_cursor_in_video(7060, true);
        set_pending(7060, 15).expect("15 is a supported choice");
        clear(7060);
        assert!(!cursor_in_video(7060));
        assert_eq!(pending(7060), DEFAULT_SHARE_FPS);
        set_cursor_in_video(7060, true);
        drop(clear_on_failure(7060));
        assert!(!cursor_in_video(7060));
    }

    #[test]
    fn the_options_report_the_cursor_choice_for_the_window() {
        clear(7070);
        assert!(!options_for_geometry(7070, 1920, 1080, 30, false).cursor_in_video);
        set_cursor_in_video(7070, true);
        assert!(options_for_geometry(7070, 1920, 1080, 30, false).cursor_in_video);
        // A live share reports the value its capture started with.
        assert!(options_for_geometry(7070, 1920, 1080, 30, true).cursor_in_video);
        clear(7070);
    }

    #[test]
    fn a_shared_window_reports_its_effective_cadence_with_every_entry_inert() {
        let shared = options_for_geometry(7100, 1920, 1080, 60, true);
        assert_eq!(shared.window_id, 7100);
        assert_eq!(shared.selected_fps, 60);
        assert_eq!(
            shared
                .choices
                .iter()
                .map(|choice| (choice.fps, choice.enabled))
                .collect::<Vec<_>>(),
            vec![(15, false), (30, false), (60, false)]
        );

        // A share whose window grew past the ceiling reports the dropped
        // cadence, and the entry that caused it stays visible.
        let dropped = options_for_geometry(7100, 4096, 2304, 30, true);
        assert_eq!(dropped.selected_fps, 30);
        assert!(dropped
            .choices
            .iter()
            .any(|choice| choice.fps == 60 && !choice.enabled));
    }

    #[test]
    fn an_unshared_window_reports_the_stored_choice_and_its_feasibility() {
        let options = options_for_geometry(7101, 1920, 1080, 30, false);
        assert_eq!(options.selected_fps, 30);
        assert!(options.choices.iter().all(|choice| choice.enabled));

        // An infeasible stored value reports the default rather than a
        // selection the share cannot honour.
        assert_eq!(
            options_for_geometry(7101, 4096, 2304, 60, false).selected_fps,
            30
        );
        let sixty = options_for_geometry(7101, 1920, 1080, 60, false);
        assert_eq!(sixty.selected_fps, 60);
        assert!(sixty.choices.iter().all(|choice| choice.enabled));
    }

    /// Petal View's estimate must be the canvas the region capture actually
    /// publishes, not the display's own dimensions: a 720p ROI on a 5K monitor
    /// is 720p.
    #[test]
    fn a_region_estimate_is_the_canvas_the_region_capture_publishes() {
        use crate::region_window::{RegionDisplay, RegionRect};

        // `scale: 1.0` is what Windows reports: per-monitor-v2 selector
        // coordinates and Win32 monitor bounds are both physical pixels.
        let display = RegionDisplay {
            id: 1,
            frame: RegionRect::new(0.0, 0.0, 5120.0, 2880.0),
            scale: 1.0,
        };
        let canvas = display
            .clipped_physical_roi(RegionRect::new(64.0, 64.0, 1280.0, 720.0))
            .expect("a selector inside its display has a canvas");
        assert_eq!((canvas.output_width, canvas.output_height), (1280, 720));
        assert!(
            effective_choices(canvas.output_width, canvas.output_height)
                .iter()
                .all(|choice| choice.enabled),
            "a 720p ROI must keep 60 fps offered even on a 5K display"
        );
    }
}
