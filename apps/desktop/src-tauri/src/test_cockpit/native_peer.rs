//! Native Test Client (test-peer) support for SHARE-01 / SHARE-N2N.
//!
//! SHARE-01 (the project history, feature A, P0) is the ONE journey that
//! validates Petal's defining feature: a shared window rendering on the
//! receiver as a real, independently movable, borderless NATIVE window — not a
//! web DOM tile. The only way to prove that on one Mac is a SECOND native
//! instance (the "Native Test Client" / test-peer) as the receiver, alongside
//! the primary instance as the sharer, both joined to the same prod room.
//!
//! ## Test-peer build (zero Rust source changes)
//!
//! `tauri-plugin-single-instance` locks on `/tmp/<identifier>_si.sock`, where
//! `<identifier>` is compiled in from `tauri.conf.json` — it is NOT
//! LaunchServices/PID based, so launching `target/debug/desktop` twice does not
//! dodge it. The fix is a wholly separate binary built with a different
//! `TAURI_CONFIG` identifier + its own `CARGO_TARGET_DIR`:
//!
//! ```sh
//! CARGO_TARGET_DIR=apps/desktop/src-tauri/target-peer \
//!   TAURI_CONFIG='{"identifier":"com.petal.app.testpeer"}' \
//!   cargo build --manifest-path apps/desktop/src-tauri/Cargo.toml
//! ```
//!
//! → `target-peer/debug/desktop`: its own single-instance socket, its own
//! `app_data_dir()`, and its own TCC identity (Screen Recording grant is
//! per-binary-path+signature, one-time). Window-source self-exclusion is
//! already PID-based (`window_source.rs`), so each instance already sees the
//! other's windows as shareable sources with no changes. See
//! `scripts/build-test-peer.sh` and `scripts/cockpit-setup.sh`.
//!
//! ## The move oracle (this module's tested logic)
//!
//! Per the plan, the "receiver renders a real native window, moved
//! independently" assertion uses a WindowServer geometry query
//! (`CGWindowListCopyWindowInfo` via `platform::cg::frame_for_window_id`)
//! sampled BEFORE and AFTER a programmatic move — that is the PRIMARY oracle,
//! not a screenshot. `screencapture -l<windowid>` (crispness) is supporting
//! evidence only. The pure decision logic below (translation + independence)
//! is unit-tested; the live sampling wrappers delegate to the shared
//! CoreGraphics primitives.

use crate::platform::cg::WindowFrame;

/// Let AppKit and WindowServer settle the newly-created compositor panel
/// between the two observations that authorize the parent to move it.
pub(crate) const RECEIVER_READINESS_SAMPLE_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(75);

#[derive(Clone, Debug, PartialEq)]
struct ReceiverReadinessObservation {
    panel_label: String,
    cg_window_id: u32,
    frame: WindowFrame,
}

impl ReceiverReadinessObservation {
    fn from_binding(
        binding: &crate::compositor::CockpitRemoteWindowBinding,
    ) -> Result<Self, &'static str> {
        if binding.frames_display_enqueued == 0 {
            return Err("no display frame has been enqueued");
        }
        if binding.panel_label.is_empty() {
            return Err("receiver panel label is empty");
        }
        if binding.cg_window_id == 0 {
            return Err("receiver CGWindowID is zero");
        }
        if binding.frame.width <= 0 || binding.frame.height <= 0 {
            return Err("receiver WindowServer geometry is zero-sized");
        }
        Ok(Self {
            panel_label: binding.panel_label.clone(),
            cg_window_id: binding.cg_window_id,
            frame: binding.frame,
        })
    }
}

/// Fail-closed readiness gate for the native receiver. A decoded/enqueued
/// frame alone is not enough: newly-created AppKit panels can briefly expose
/// transient or zero WindowServer geometry. The parent may move the panel only
/// after two consecutive, matching, real WindowServer observations.
pub(crate) struct ReceiverReadinessTracker {
    previous: Option<ReceiverReadinessObservation>,
    last_pending_reason: String,
}

impl ReceiverReadinessTracker {
    pub(crate) fn new() -> Self {
        Self {
            previous: None,
            last_pending_reason: "no receiver binding observed".to_string(),
        }
    }

    /// Returns `true` only on the second consecutive stable observation.
    pub(crate) fn observe(
        &mut self,
        binding: &crate::compositor::CockpitRemoteWindowBinding,
    ) -> bool {
        let observation = match ReceiverReadinessObservation::from_binding(binding) {
            Ok(observation) => observation,
            Err(reason) => {
                self.previous = None;
                self.last_pending_reason = reason.to_string();
                return false;
            }
        };
        if self.previous.as_ref() == Some(&observation) {
            return true;
        }
        self.last_pending_reason = match self.previous.as_ref() {
            Some(previous) => format!(
                "receiver geometry still changing: {}:{} {:?} -> {}:{} {:?}",
                previous.panel_label,
                previous.cg_window_id,
                previous.frame,
                observation.panel_label,
                observation.cg_window_id,
                observation.frame,
            ),
            None => "waiting for a second stable receiver geometry sample".to_string(),
        };
        self.previous = Some(observation);
        false
    }

    /// An inspection failure interrupts the consecutive-sample proof. Keeping
    /// the prior observation would allow A/error/A to masquerade as two stable
    /// samples even though WindowServer continuity was not observed.
    pub(crate) fn observe_error(&mut self, error: &str) {
        self.previous = None;
        self.last_pending_reason = format!("receiver inspection failed: {error}");
    }

    pub(crate) fn timeout_error(&self, owner_identity: &str, source_window_id: u32) -> String {
        format!(
            "native peer compositor did not reach stable receiver readiness for {owner_identity}:{source_window_id}: {}",
            self.last_pending_reason
        )
    }
}

/// One fresh sharer-side WindowServer observation, with enough context to
/// distinguish a genuinely absent window from a window which macOS temporarily
/// removed from the on-screen list. `appkit_frame` is diagnostic only: the
/// independent-move oracle always consumes `on_screen_frame`.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SharerFrameSample {
    pub attempt: u8,
    pub on_screen_frame: Option<WindowFrame>,
    pub exists_in_all_windows: bool,
    pub owner_pid: Option<i32>,
    pub frontmost_app: String,
    pub petal_active: bool,
    pub appkit_frame: Option<WindowFrame>,
}

/// Select the first freshly observed on-screen frame from bounded attempts.
/// The caller owns timing between attempts; this pure helper makes it
/// impossible to silently replace absent evidence with AppKit/cached geometry.
pub(crate) fn first_fresh_on_screen_sample(
    samples: impl IntoIterator<Item = SharerFrameSample>,
) -> SharerFrameSample {
    samples
        .into_iter()
        .find(|sample| sample.on_screen_frame.is_some())
        .unwrap_or_else(|| SharerFrameSample {
            attempt: 0,
            on_screen_frame: None,
            exists_in_all_windows: false,
            owner_pid: None,
            frontmost_app: "sampler produced no observations".to_string(),
            petal_active: false,
            appkit_frame: None,
        })
}

pub(crate) fn sharer_sample_classification(samples: &[SharerFrameSample]) -> &'static str {
    if samples.is_empty() {
        "zero-observations"
    } else if samples
        .iter()
        .any(|sample| sample.on_screen_frame.is_some())
    {
        "on-screen-frame-observed"
    } else {
        "on-screen-frame-missing-after-attempts"
    }
}

/// Bundle identifier the test-peer binary is built with. Distinct from the
/// primary `com.petal.app` so the single-instance socket, `app_data_dir()`,
/// and TCC identity are all separate. Must match `scripts/build-test-peer.sh`
/// and `scripts/cockpit-setup.sh`.
pub(crate) const TEST_PEER_IDENTIFIER: &str = "com.petal.app.testpeer";

/// `CARGO_TARGET_DIR` subdir (under the desktop crate dir) the test-peer builds
/// into, keeping its object tree fully separate from the primary `target/`.
/// Must match the build scripts.
pub(crate) const TEST_PEER_TARGET_SUBDIR: &str = "target-peer";

/// Path of the built test-peer executable, relative to the desktop crate dir
/// (`apps/desktop/src-tauri`). Must match the build scripts.
pub(crate) const TEST_PEER_BIN_RELATIVE: &str = "target-peer/debug/desktop";

/// Max points the observed move delta may differ from the requested delta and
/// still count as landing where asked. WindowServer reports integer-rounded
/// point frames, so a couple points of slack absorbs rounding.
pub(crate) const MOVE_TOLERANCE_PX: i32 = 2;

/// Smallest programmatic move (sum of per-axis magnitudes) we will request and
/// assert on. Comfortably above the tolerance so a real translation is
/// unambiguous and a no-op "move" cannot false-pass.
pub(crate) const MIN_MOVE_PX: i32 = 40;

/// Assert a receiver window underwent a PURE TRANSLATION by (requested_dx,
/// requested_dy): same size, moved by (approximately) the requested delta, and
/// the requested delta was non-trivial. Returns the observed (dx, dy).
///
/// This is the core of "independently draggable": a real native window's frame
/// origin follows a programmatic move while its size is unchanged.
#[allow(dead_code)]
pub(crate) fn assert_pure_translation(
    before: WindowFrame,
    after: WindowFrame,
    requested_dx: i32,
    requested_dy: i32,
) -> Result<(i32, i32), String> {
    if requested_dx.abs() + requested_dy.abs() < MIN_MOVE_PX {
        return Err(format!(
            "requested move delta ({requested_dx},{requested_dy}) is below the {MIN_MOVE_PX}px minimum; a no-op move cannot prove independent draggability"
        ));
    }
    if after.width != before.width || after.height != before.height {
        return Err(format!(
            "receiver window changed size during the move ({}x{} -> {}x{}); expected a pure translation, not a resize",
            before.width, before.height, after.width, after.height
        ));
    }
    let dx = after.x - before.x;
    let dy = after.y - before.y;
    if (dx - requested_dx).abs() > MOVE_TOLERANCE_PX
        || (dy - requested_dy).abs() > MOVE_TOLERANCE_PX
    {
        return Err(format!(
            "receiver window moved by ({dx},{dy}) but ({requested_dx},{requested_dy}) was requested (tolerance {MOVE_TOLERANCE_PX}px)"
        ));
    }
    Ok((dx, dy))
}

/// Assert the receiver window's move was INDEPENDENT of the sharer's own source
/// window. The receiver is a separate native window, so moving it must NOT drag
/// the sharer's source window along. `sharer_before`/`sharer_after` are the
/// sharer source window's frames sampled over the same interval; a real
/// independent window leaves them (approximately) unchanged.
#[allow(dead_code)]
pub(crate) fn assert_independent_of_sharer(
    sharer_before: WindowFrame,
    sharer_after: WindowFrame,
) -> Result<(), String> {
    let sdx = sharer_after.x - sharer_before.x;
    let sdy = sharer_after.y - sharer_before.y;
    if sdx.abs() > MOVE_TOLERANCE_PX || sdy.abs() > MOVE_TOLERANCE_PX {
        return Err(format!(
            "sharer source window also moved by ({sdx},{sdy}) while the receiver window was moved; the receiver is not an independent window"
        ));
    }
    Ok(())
}

/// Full move oracle: given the receiver window's before/after frames around a
/// programmatic move of (requested_dx, requested_dy), and the sharer source
/// window's before/after frames, decide whether the receiver
/// rendered as a real, independently movable native window. Returns a
/// human-readable detail on success and a specific reason on failure.
#[allow(dead_code)]
pub(crate) fn evaluate_independent_move(
    receiver_before: WindowFrame,
    receiver_after: WindowFrame,
    requested_dx: i32,
    requested_dy: i32,
    sharer_frames: (WindowFrame, WindowFrame),
) -> Result<String, String> {
    let (dx, dy) =
        assert_pure_translation(receiver_before, receiver_after, requested_dx, requested_dy)?;
    let (sharer_before, sharer_after) = sharer_frames;
    assert_independent_of_sharer(sharer_before, sharer_after)?;
    Ok(format!(
        "receiver window translated by ({dx},{dy}) with size preserved ({}x{}); sharer source window stayed put (independent)",
        receiver_after.width,
        receiver_after.height,
    ))
}

/// Both sharer frame samples are mandatory for the defining native-to-native
/// pass. A missing sample is an evidence gap, not permission to quietly omit
/// the independent-window assertion.
pub(crate) fn require_sharer_frame_samples(
    before: Option<WindowFrame>,
    after: Option<WindowFrame>,
) -> Result<(WindowFrame, WindowFrame), String> {
    match (before, after) {
        (Some(before), Some(after)) => Ok((before, after)),
        (None, None) => Err(
            "missing both sharer WindowServer frame samples; cannot prove receiver move was independent"
                .to_string(),
        ),
        (None, Some(_)) => Err(
            "missing pre-move sharer WindowServer frame sample; cannot prove receiver move was independent"
                .to_string(),
        ),
        (Some(_), None) => Err(
            "missing post-move sharer WindowServer frame sample; cannot prove receiver move was independent"
                .to_string(),
        ),
    }
}

/// A move must act on the already-rendering native panel. If its label or
/// CGWindowID changes, a replacement panel could make a geometry-only check
/// look like a successful move.
pub(crate) fn assert_same_receiver_surface(
    before_panel_label: &str,
    before_cg_window_id: u32,
    after_panel_label: &str,
    after_cg_window_id: u32,
) -> Result<(), String> {
    if before_panel_label != after_panel_label {
        return Err(format!(
            "receiver panel changed during move ('{before_panel_label}' -> '{after_panel_label}'); expected the same native panel"
        ));
    }
    if before_cg_window_id != after_cg_window_id {
        return Err(format!(
            "receiver CGWindowID changed during move ({before_cg_window_id} -> {after_cg_window_id}); expected the same native panel"
        ));
    }
    Ok(())
}

/// Live sample of a window's current on-screen frame by CGWindowID, for the
/// before/after readings around a programmatic move. Thin wrapper over the
/// shared CoreGraphics primitive (macOS); `None` off-platform.
#[allow(dead_code)]
pub(crate) fn window_frame(window_id: u32) -> Option<WindowFrame> {
    crate::platform::cg::frame_for_window_id(window_id)
}

/// Whether a window id still exists in CoreGraphics' full `OptionAll` list.
/// This is diagnostic classification only; it is deliberately not an
/// on-screen-frame substitute for the SHARE-01 oracle.
#[allow(dead_code)]
pub(crate) fn window_exists_in_all_windows(window_id: u32) -> bool {
    crate::platform::cg::window_exists(window_id)
}

/// Find the receiver's compositor window owned by the test-peer process. The
/// peer renders each remote share as a borderless native panel; this scans the
/// global window list for an on-screen, non-trivially-sized window owned by
/// `peer_pid`. Returns its CGWindowID. macOS-only (the raw window list is a
/// macOS primitive).
#[cfg(target_os = "macos")]
#[allow(dead_code)]
pub(crate) fn find_receiver_compositor_window(peer_pid: i64) -> Option<u32> {
    let entries = crate::platform::cg::onscreen_windows()?;
    entries
        .into_iter()
        .filter(|entry| entry.owner_pid == peer_pid)
        // Skip zero/degenerate and transparent chrome; the compositor remote
        // window is a real content-sized surface.
        .filter(|entry| {
            entry.number > 0 && entry.alpha > 0.01 && entry.w >= 80.0 && entry.h >= 80.0
        })
        .map(|entry| entry.number)
        .find(|number| *number > 0)
        .and_then(|number| u32::try_from(number).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(x: i32, y: i32, width: i32, height: i32) -> WindowFrame {
        WindowFrame {
            x,
            y,
            width,
            height,
        }
    }

    fn binding(
        panel_label: &str,
        cg_window_id: u32,
        frame: WindowFrame,
        frames_display_enqueued: u64,
    ) -> crate::compositor::CockpitRemoteWindowBinding {
        crate::compositor::CockpitRemoteWindowBinding {
            owner_identity: "sharer".to_string(),
            source_window_id: 42,
            panel_label: panel_label.to_string(),
            cg_window_id,
            frame,
            frames_received: frames_display_enqueued,
            frames_display_enqueued,
        }
    }

    #[test]
    fn receiver_readiness_waits_through_zero_geometry_then_stabilizes() {
        let mut tracker = ReceiverReadinessTracker::new();
        let zero = binding("panel-a", 17, frame(100, 200, 0, 0), 1);
        let stable = binding("panel-a", 17, frame(100, 200, 640, 480), 2);

        assert!(!tracker.observe(&zero));
        assert!(!tracker.observe(&stable));
        assert!(tracker.observe(&stable));
    }

    #[test]
    fn receiver_readiness_resets_when_windowserver_frame_changes() {
        let mut tracker = ReceiverReadinessTracker::new();
        let first = binding("panel-a", 17, frame(100, 200, 640, 480), 1);
        let changed = binding("panel-a", 17, frame(101, 200, 640, 480), 2);

        assert!(!tracker.observe(&first));
        assert!(!tracker.observe(&changed));
        assert!(tracker.observe(&changed));
    }

    #[test]
    fn receiver_readiness_requires_two_matching_surface_samples() {
        let mut tracker = ReceiverReadinessTracker::new();
        let stable = binding("panel-a", 17, frame(100, 200, 640, 480), 1);

        assert!(!tracker.observe(&stable));
        assert!(tracker.observe(&stable));
    }

    #[test]
    fn receiver_readiness_timeout_remains_fail_closed() {
        let mut tracker = ReceiverReadinessTracker::new();
        let no_enqueued_frame = binding("panel-a", 17, frame(100, 200, 640, 480), 0);

        assert!(!tracker.observe(&no_enqueued_frame));
        let error = tracker.timeout_error("sharer", 42);
        assert!(error.contains("did not reach stable receiver readiness"));
        assert!(error.contains("no display frame has been enqueued"));
    }

    #[test]
    fn receiver_readiness_error_breaks_consecutive_stability() {
        let mut tracker = ReceiverReadinessTracker::new();
        let stable = binding("panel-a", 17, frame(100, 200, 640, 480), 1);

        assert!(!tracker.observe(&stable));
        tracker.observe_error("WindowServer cannot see compositor panel 17");
        assert!(!tracker.observe(&stable));
        assert!(tracker.observe(&stable));
    }

    #[test]
    fn receiver_readiness_timeout_reports_last_inspection_error() {
        let mut tracker = ReceiverReadinessTracker::new();
        tracker.observe_error("timed out inspecting compositor panel on main thread");

        let error = tracker.timeout_error("sharer", 42);
        assert!(error.contains("receiver inspection failed"));
        assert!(error.contains("timed out inspecting compositor panel on main thread"));
    }

    #[test]
    fn pure_translation_accepts_exact_move() {
        let before = frame(100, 200, 640, 480);
        let after = frame(220, 200, 640, 480);
        assert_eq!(assert_pure_translation(before, after, 120, 0), Ok((120, 0)));
    }

    #[test]
    fn pure_translation_tolerates_rounding_within_two_px() {
        let before = frame(100, 200, 640, 480);
        // WindowServer rounded the landing point by 1px on each axis.
        let after = frame(219, 261, 640, 480);
        assert_eq!(
            assert_pure_translation(before, after, 120, 60),
            Ok((119, 61))
        );
    }

    #[test]
    fn pure_translation_rejects_a_resize() {
        let before = frame(100, 200, 640, 480);
        let after = frame(220, 200, 641, 480);
        let err = assert_pure_translation(before, after, 120, 0).unwrap_err();
        assert!(err.contains("changed size"), "unexpected: {err}");
    }

    #[test]
    fn pure_translation_rejects_wrong_delta() {
        let before = frame(100, 200, 640, 480);
        let after = frame(150, 200, 640, 480);
        let err = assert_pure_translation(before, after, 120, 0).unwrap_err();
        assert!(err.contains("moved by (50,0)"), "unexpected: {err}");
    }

    #[test]
    fn pure_translation_rejects_noop_request() {
        let before = frame(100, 200, 640, 480);
        let after = frame(100, 200, 640, 480);
        let err = assert_pure_translation(before, after, 4, 4).unwrap_err();
        assert!(err.contains("below the"), "unexpected: {err}");
    }

    #[test]
    fn independence_passes_when_sharer_stays_put() {
        let sharer = frame(10, 20, 800, 600);
        assert_eq!(assert_independent_of_sharer(sharer, sharer), Ok(()));
    }

    #[test]
    fn independence_fails_when_sharer_tracks_the_move() {
        let before = frame(10, 20, 800, 600);
        let after = frame(130, 20, 800, 600);
        let err = assert_independent_of_sharer(before, after).unwrap_err();
        assert!(
            err.contains("not an independent window"),
            "unexpected: {err}"
        );
    }

    #[test]
    fn evaluate_full_oracle_pass_with_independence() {
        let rb = frame(100, 100, 640, 480);
        let ra = frame(260, 100, 640, 480);
        let sb = frame(0, 0, 900, 700);
        let sa = frame(1, 0, 900, 700);
        let detail = evaluate_independent_move(rb, ra, 160, 0, (sb, sa)).unwrap();
        assert!(
            detail.contains("translated by (160,0)"),
            "unexpected: {detail}"
        );
        assert!(detail.contains("independent"), "unexpected: {detail}");
    }

    #[test]
    fn evaluate_full_oracle_reports_dependence() {
        let rb = frame(100, 100, 640, 480);
        let ra = frame(260, 100, 640, 480);
        let sb = frame(0, 0, 900, 700);
        let sa = frame(160, 0, 900, 700);
        let err = evaluate_independent_move(rb, ra, 160, 0, (sb, sa)).unwrap_err();
        assert!(
            err.contains("not an independent window"),
            "unexpected: {err}"
        );
    }

    #[test]
    fn require_sharer_frame_samples_rejects_each_missing_evidence_case() {
        let sample = frame(0, 0, 900, 700);
        for (before, after, expected) in [
            (None, None, "both"),
            (None, Some(sample), "pre-move"),
            (Some(sample), None, "post-move"),
        ] {
            let error = require_sharer_frame_samples(before, after).unwrap_err();
            assert!(error.contains(expected), "unexpected: {error}");
        }
    }

    #[test]
    fn fresh_sampler_retries_until_an_on_screen_frame_arrives() {
        let missing = SharerFrameSample {
            attempt: 1,
            on_screen_frame: None,
            exists_in_all_windows: true,
            owner_pid: Some(42),
            frontmost_app: "Petal".to_string(),
            petal_active: true,
            appkit_frame: Some(frame(1, 2, 960, 600)),
        };
        let present = SharerFrameSample {
            attempt: 2,
            on_screen_frame: Some(frame(1, 2, 960, 600)),
            ..missing.clone()
        };
        let selected = first_fresh_on_screen_sample([missing, present]);
        assert_eq!(selected.attempt, 2);
        assert_eq!(selected.on_screen_frame, Some(frame(1, 2, 960, 600)));
    }

    #[test]
    fn fresh_sampler_never_substitutes_appkit_frame_for_missing_oracle_evidence() {
        let missing = SharerFrameSample {
            attempt: 4,
            on_screen_frame: None,
            exists_in_all_windows: true,
            owner_pid: Some(42),
            frontmost_app: "Petal".to_string(),
            petal_active: true,
            appkit_frame: Some(frame(1, 2, 960, 600)),
        };
        let selected = first_fresh_on_screen_sample([missing]);
        assert!(selected.on_screen_frame.is_none());
        assert_eq!(
            require_sharer_frame_samples(selected.on_screen_frame, None).unwrap_err(),
            "missing both sharer WindowServer frame samples; cannot prove receiver move was independent"
        );
    }

    #[test]
    fn zero_sampler_observations_are_distinct_from_on_screen_misses() {
        assert_eq!(sharer_sample_classification(&[]), "zero-observations");
    }

    #[test]
    fn receiver_surface_must_remain_the_same_panel_and_window() {
        assert_eq!(
            assert_same_receiver_surface("panel-a", 17, "panel-a", 17),
            Ok(())
        );
        let label_error = assert_same_receiver_surface("panel-a", 17, "panel-b", 17).unwrap_err();
        assert!(
            label_error.contains("panel changed"),
            "unexpected: {label_error}"
        );
        let window_error = assert_same_receiver_surface("panel-a", 17, "panel-a", 18).unwrap_err();
        assert!(
            window_error.contains("CGWindowID changed"),
            "unexpected: {window_error}"
        );
    }
}
