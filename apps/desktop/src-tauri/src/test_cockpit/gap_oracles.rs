//! Pure-logic pass-criteria oracles for the P-3 gap journeys
//! (the project history): SHARE-05 (multi-window), SHARE-06/10
//! (multi-display / full desktop share, geometry-only helpers reused from
//! `native_peer`), CAM-03 (bitrate scaling), CAM-04 (camera stall), ROOM-01
//! (roster match), PTR-02 (bidirectional draw), and UI-01..04 (text overflow).
//!
//! ## Why this module exists (honesty contract)
//!
//! Exactly like `native_peer` for SHARE-01, these are the DECISION functions
//! each journey's eventual live verdict will call — extracted as pure,
//! deterministic logic so they can be genuinely unit-tested headlessly (the
//! cargo-test gate), independently of the live two-peer / native-window /
//! camera-telemetry orchestration that is NOT yet auto-driven. The runnable
//! scenarios in `mod.rs` therefore preflight and return INFRA-FAIL rather than
//! false-pass; the oracle logic they will use is already covered here. A
//! journey with a tested oracle + an honest INFRA-FAIL scaffold is "partial",
//! never "covered".

/// SPEC.md §4.3 concurrent-share cap ("4 windows per user"), mirrored from
/// `session::share::MAX_CONCURRENT_SHARES`. Kept as a local const so the oracle
/// is self-contained and testable without the session layer.
pub(crate) const MAX_CONCURRENT_SHARES: usize = 4;

/// Minimum fps the single FOCUSED share must sustain to count as "full". Below
/// this the focus policy has failed to keep the focused window live.
pub(crate) const FOCUSED_FULL_FPS_FLOOR: f64 = 20.0;

/// One shared window as observed on the receiver: whether it is the focused
/// share and its delivered fps.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ShareSample {
    pub focused: bool,
    pub fps: f64,
}

/// SHARE-05 (Multi-window): given every share a sharer is publishing at once,
/// decide whether the focus-weighted cap held — the whole point of the journey:
///
/// * no more than `MAX_CONCURRENT_SHARES` shares (the spec cap),
/// * exactly one focused share, streaming at full fps,
/// * every NON-focused share is still LIVE (fps > 0 — a glanceable low-fps
///   layer), never dropped to a dead 0-fps track ("the others dying"), and not
///   exceeding the focused share's fps (glanceable, not full),
/// * therefore no keyframe storm starved a non-focused share to death.
///
/// Returns a human-readable detail on success, a specific reason on failure.
#[allow(dead_code)]
pub(crate) fn evaluate_focus_weighted_cap(shares: &[ShareSample]) -> Result<String, String> {
    if shares.is_empty() {
        return Err("no shares sampled; multi-window cap cannot be evaluated".to_string());
    }
    if shares.len() > MAX_CONCURRENT_SHARES {
        return Err(format!(
            "{} concurrent shares exceeds the {MAX_CONCURRENT_SHARES}-window cap (SPEC §4.3)",
            shares.len()
        ));
    }
    let focused: Vec<&ShareSample> = shares.iter().filter(|share| share.focused).collect();
    if focused.len() != 1 {
        return Err(format!(
            "expected exactly one focused share, found {}",
            focused.len()
        ));
    }
    let focused_fps = focused[0].fps;
    if focused_fps < FOCUSED_FULL_FPS_FLOOR {
        return Err(format!(
            "focused share fps {focused_fps:.1} is below the full-fps floor {FOCUSED_FULL_FPS_FLOOR:.1}"
        ));
    }
    for (index, share) in shares.iter().enumerate() {
        if share.focused {
            continue;
        }
        if share.fps <= 0.0 {
            return Err(format!(
                "non-focused share #{index} died (fps {:.1}); a keyframe storm or cap starved it instead of keeping it glanceable-live",
                share.fps
            ));
        }
        // A non-focused share may momentarily blip to the focused rate, but a
        // sustained value materially ABOVE the focused share means the cap is
        // not actually demoting it. Allow a small slack for sampling jitter.
        if share.fps > focused_fps + 2.0 {
            return Err(format!(
                "non-focused share #{index} fps {:.1} exceeds the focused share ({focused_fps:.1}); it was never demoted to a glanceable layer",
                share.fps
            ));
        }
    }
    Ok(format!(
        "{} shares within cap; focused streaming at {:.1} fps; {} non-focused share(s) stayed glanceable-live",
        shares.len(),
        focused_fps,
        shares.len() - 1
    ))
}

/// Expected camera bitrate band (kbps) for a delivered resolution tier. Mirrors
/// `transport::publisher`'s camera encoding ladder: Full ≈ 1.5 Mbps at 720p, a
/// half layer ≈ 0.5 Mbps at 360p. Bands are inclusive `[min, max]`.
#[allow(dead_code)]
pub(crate) fn expected_camera_kbps_band(width: u32, height: u32) -> (f64, f64) {
    let area = u64::from(width) * u64::from(height);
    if area >= 1280 * 720 {
        (400.0, 1600.0)
    } else if area >= 640 * 360 {
        (100.0, 550.0)
    } else {
        (40.0, 300.0)
    }
}

/// CAM-03 (Bitrate scaling, #246): assert the camera's measured bitrate TRACKS
/// its resolution tier, not merely that fps > 0. Rejects a track that is
/// nominally live but publishing far below (or above) the band its tier
/// implies — the exact regression #246 calls out.
#[allow(dead_code)]
pub(crate) fn assert_bitrate_tracks_tier(
    width: u32,
    height: u32,
    measured_kbps: f64,
) -> Result<String, String> {
    if width == 0 || height == 0 {
        return Err(
            "camera track reported a zero dimension; cannot judge bitrate tier".to_string(),
        );
    }
    if measured_kbps <= 0.0 {
        return Err(format!(
            "camera bitrate {measured_kbps:.1} kbps is not positive; track is not really publishing"
        ));
    }
    let (min_kbps, max_kbps) = expected_camera_kbps_band(width, height);
    if measured_kbps < min_kbps {
        return Err(format!(
            "camera bitrate {measured_kbps:.1} kbps is below the {min_kbps:.0}-{max_kbps:.0} kbps band expected for {width}x{height}; bitrate is not tracking the tier (fps>0 alone is insufficient, #246)"
        ));
    }
    if measured_kbps > max_kbps {
        return Err(format!(
            "camera bitrate {measured_kbps:.1} kbps exceeds the {min_kbps:.0}-{max_kbps:.0} kbps band expected for {width}x{height}"
        ));
    }
    Ok(format!(
        "camera bitrate {measured_kbps:.1} kbps is within the {min_kbps:.0}-{max_kbps:.0} kbps band for {width}x{height}"
    ))
}

/// A time-sampled decoded-frame count for the camera stall watchdog.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct FrameSample {
    pub t_ms: u128,
    pub frames_decoded: u64,
}

/// CAM-04 (Camera stall, #247): the no-new-frame-for-N watchdog. Given
/// decoded-frame counts sampled over time, fail if the count fails to advance
/// for longer than `stall_ms` (a frozen tile), pass if frames keep advancing.
/// Mirrors the window-share inter-frame-gap watchdog, applied to the gallery
/// camera tile.
#[allow(dead_code)]
pub(crate) fn detect_camera_stall(
    samples: &[FrameSample],
    stall_ms: u128,
) -> Result<String, String> {
    if samples.len() < 2 {
        return Err(format!(
            "only {} camera frame sample(s); need at least 2 to judge a stall",
            samples.len()
        ));
    }
    let mut last_advance_t = samples[0].t_ms;
    let mut last_count = samples[0].frames_decoded;
    let mut max_gap: u128 = 0;
    for sample in &samples[1..] {
        if sample.frames_decoded > last_count {
            last_count = sample.frames_decoded;
            last_advance_t = sample.t_ms;
        } else {
            let gap = sample.t_ms.saturating_sub(last_advance_t);
            max_gap = max_gap.max(gap);
            if gap > stall_ms {
                return Err(format!(
                    "camera tile stalled: no new decoded frame for {gap} ms (> {stall_ms} ms watchdog); frozen at {last_count} frames"
                ));
            }
        }
    }
    Ok(format!(
        "camera tile advanced with a max inter-frame gap of {max_gap} ms (< {stall_ms} ms watchdog)"
    ))
}

/// A UI element's measured layout box for the text-overflow check.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TextBox {
    pub name: String,
    pub scroll_width: f64,
    pub client_width: f64,
}

/// UI-01..04: assert NO user-facing text overflows its container — the hard
/// "UI text must never truncate" rule expressed as `scrollWidth <= clientWidth`
/// for every measured element (sub-pixel slack allowed). Fails naming the first
/// clipped element; refuses to pass on an empty measurement set (nothing
/// measured proves nothing).
#[allow(dead_code)]
pub(crate) fn assert_no_text_overflow(elements: &[TextBox]) -> Result<String, String> {
    const OVERFLOW_SLACK_PX: f64 = 0.5;
    if elements.is_empty() {
        return Err("no UI elements measured; cannot prove text is not truncated".to_string());
    }
    for element in elements {
        if element.scroll_width > element.client_width + OVERFLOW_SLACK_PX {
            return Err(format!(
                "text overflow in '{}': scrollWidth {:.1} > clientWidth {:.1} (text is clipped/truncated — violates the UI-text hard rule)",
                element.name, element.scroll_width, element.client_width
            ));
        }
    }
    Ok(format!(
        "all {} measured element(s) fit (scrollWidth <= clientWidth)",
        elements.len()
    ))
}

/// ROOM-01 (Join room): assert the roster is consistent across sides — the set
/// of participant identities the native client sees must equal the set the web
/// client sees. Fails naming who is missing on which side; refuses to pass on
/// an empty roster (a room with nobody in it proves nothing about presence).
#[allow(dead_code)]
pub(crate) fn assert_rosters_match(native: &[String], web: &[String]) -> Result<String, String> {
    use std::collections::BTreeSet;
    let native_set: BTreeSet<&String> = native.iter().collect();
    let web_set: BTreeSet<&String> = web.iter().collect();
    if native_set.is_empty() || web_set.is_empty() {
        return Err(format!(
            "roster is empty on at least one side (native={}, web={}); cannot confirm presence",
            native_set.len(),
            web_set.len()
        ));
    }
    let missing_on_web: Vec<&&String> = native_set.difference(&web_set).collect();
    let missing_on_native: Vec<&&String> = web_set.difference(&native_set).collect();
    if !missing_on_web.is_empty() || !missing_on_native.is_empty() {
        return Err(format!(
            "roster mismatch: missing on web={missing_on_web:?}, missing on native={missing_on_native:?}"
        ));
    }
    Ok(format!(
        "roster matches on all sides ({} participant(s))",
        native_set.len()
    ))
}

/// PTR-02 (Draw stroke, both directions): the journey's bar is that a stroke is
/// delivered in BOTH directions — native→web AND web→native. Given each
/// direction's delivery result, pass only when both are confirmed; name the
/// missing direction otherwise.
#[allow(dead_code)]
pub(crate) fn assert_bidirectional_draw(
    native_to_web: bool,
    web_to_native: bool,
) -> Result<String, String> {
    match (native_to_web, web_to_native) {
        (true, true) => {
            Ok("draw stroke delivered in both directions (native→web and web→native)".to_string())
        }
        (false, true) => Err("draw stroke was delivered web→native but NOT native→web".to_string()),
        (true, false) => Err("draw stroke was delivered native→web but NOT web→native".to_string()),
        (false, false) => Err("draw stroke was not delivered in either direction".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn share(focused: bool, fps: f64) -> ShareSample {
        ShareSample { focused, fps }
    }

    #[test]
    fn focus_cap_passes_one_focused_rest_glanceable() {
        let shares = [
            share(true, 30.0),
            share(false, 4.0),
            share(false, 4.0),
            share(false, 2.0),
        ];
        let detail = evaluate_focus_weighted_cap(&shares).unwrap();
        assert!(detail.contains("within cap"), "unexpected: {detail}");
        assert!(detail.contains("3 non-focused"), "unexpected: {detail}");
    }

    #[test]
    fn focus_cap_rejects_more_than_four_shares() {
        let shares = [
            share(true, 30.0),
            share(false, 4.0),
            share(false, 4.0),
            share(false, 4.0),
            share(false, 4.0),
        ];
        let err = evaluate_focus_weighted_cap(&shares).unwrap_err();
        assert!(
            err.contains("exceeds the 4-window cap"),
            "unexpected: {err}"
        );
    }

    #[test]
    fn focus_cap_rejects_dead_nonfocused_share() {
        let shares = [share(true, 30.0), share(false, 0.0)];
        let err = evaluate_focus_weighted_cap(&shares).unwrap_err();
        assert!(err.contains("died"), "unexpected: {err}");
    }

    #[test]
    fn focus_cap_rejects_no_focused_share() {
        let shares = [share(false, 4.0), share(false, 4.0)];
        let err = evaluate_focus_weighted_cap(&shares).unwrap_err();
        assert!(err.contains("exactly one focused"), "unexpected: {err}");
    }

    #[test]
    fn focus_cap_rejects_low_focused_fps() {
        let shares = [share(true, 5.0), share(false, 4.0)];
        let err = evaluate_focus_weighted_cap(&shares).unwrap_err();
        assert!(
            err.contains("below the full-fps floor"),
            "unexpected: {err}"
        );
    }

    #[test]
    fn bitrate_tracks_tier_accepts_full_720p() {
        let detail = assert_bitrate_tracks_tier(1280, 720, 1200.0).unwrap();
        assert!(detail.contains("within"), "unexpected: {detail}");
    }

    #[test]
    fn bitrate_tracks_tier_rejects_live_but_starved_track() {
        // fps>0 but bitrate is a trickle far below the 720p band (#246).
        let err = assert_bitrate_tracks_tier(1280, 720, 50.0).unwrap_err();
        assert!(err.contains("not tracking the tier"), "unexpected: {err}");
    }

    #[test]
    fn bitrate_tracks_tier_accepts_half_360p() {
        assert!(assert_bitrate_tracks_tier(640, 360, 300.0).is_ok());
    }

    #[test]
    fn bitrate_band_is_tier_ordered() {
        let (_, full_max) = expected_camera_kbps_band(1280, 720);
        let (_, half_max) = expected_camera_kbps_band(640, 360);
        assert!(
            full_max > half_max,
            "full tier must allow more bitrate than half"
        );
    }

    #[test]
    fn stall_watchdog_passes_when_frames_advance() {
        let samples = [
            FrameSample {
                t_ms: 0,
                frames_decoded: 10,
            },
            FrameSample {
                t_ms: 500,
                frames_decoded: 25,
            },
            FrameSample {
                t_ms: 1000,
                frames_decoded: 40,
            },
        ];
        let detail = detect_camera_stall(&samples, 2000).unwrap();
        assert!(detail.contains("advanced"), "unexpected: {detail}");
    }

    #[test]
    fn stall_watchdog_detects_frozen_tile() {
        let samples = [
            FrameSample {
                t_ms: 0,
                frames_decoded: 10,
            },
            FrameSample {
                t_ms: 1500,
                frames_decoded: 10,
            },
            FrameSample {
                t_ms: 3000,
                frames_decoded: 10,
            },
        ];
        let err = detect_camera_stall(&samples, 2000).unwrap_err();
        assert!(err.contains("stalled"), "unexpected: {err}");
        assert!(err.contains("frozen at 10 frames"), "unexpected: {err}");
    }

    #[test]
    fn stall_watchdog_needs_two_samples() {
        let samples = [FrameSample {
            t_ms: 0,
            frames_decoded: 10,
        }];
        assert!(detect_camera_stall(&samples, 2000).is_err());
    }

    #[test]
    fn text_overflow_passes_when_everything_fits() {
        let elements = [
            TextBox {
                name: "join-button".to_string(),
                scroll_width: 120.0,
                client_width: 140.0,
            },
            TextBox {
                name: "room-name".to_string(),
                scroll_width: 200.0,
                client_width: 200.0,
            },
        ];
        let detail = assert_no_text_overflow(&elements).unwrap();
        assert!(detail.contains("2 measured"), "unexpected: {detail}");
    }

    #[test]
    fn text_overflow_names_the_clipped_element() {
        let elements = [TextBox {
            name: "create-row".to_string(),
            scroll_width: 410.0,
            client_width: 400.0,
        }];
        let err = assert_no_text_overflow(&elements).unwrap_err();
        assert!(err.contains("create-row"), "unexpected: {err}");
        assert!(err.contains("truncated"), "unexpected: {err}");
    }

    #[test]
    fn text_overflow_refuses_empty_measurements() {
        assert!(assert_no_text_overflow(&[]).is_err());
    }

    #[test]
    fn rosters_match_when_identical() {
        let native = vec!["alice".to_string(), "bob".to_string()];
        let web = vec!["bob".to_string(), "alice".to_string()];
        let detail = assert_rosters_match(&native, &web).unwrap();
        assert!(detail.contains("matches"), "unexpected: {detail}");
    }

    #[test]
    fn rosters_mismatch_names_the_missing_side() {
        let native = vec!["alice".to_string(), "bob".to_string()];
        let web = vec!["alice".to_string()];
        let err = assert_rosters_match(&native, &web).unwrap_err();
        assert!(err.contains("mismatch"), "unexpected: {err}");
        assert!(err.contains("bob"), "unexpected: {err}");
    }

    #[test]
    fn rosters_refuse_empty_side() {
        assert!(assert_rosters_match(&[], &["alice".to_string()]).is_err());
    }

    #[test]
    fn bidirectional_draw_requires_both_directions() {
        assert!(assert_bidirectional_draw(true, true).is_ok());
        assert!(assert_bidirectional_draw(true, false)
            .unwrap_err()
            .contains("NOT web→native"));
        assert!(assert_bidirectional_draw(false, true)
            .unwrap_err()
            .contains("NOT native→web"));
        assert!(assert_bidirectional_draw(false, false).is_err());
    }
}
