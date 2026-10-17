//! SHARE-W2N-Q receiver-window oracles: what the native RECEIVER shows for a
//! browser's shared window, beyond "video frames arrive".
//!
//! * #315 docking: the control and pointer overlay children must sit on the
//!   panel's content area (`chrome_frames_for_panel_bounds`), read from the
//!   WindowServer (CGWindowList), never at the screen corner
//!   `(0, HEADER_HEIGHT)`. Field signature: children at (0,44) while the panel
//!   sat at (217,175).
//! * #315 symptom: the web peer's telepointer sweep (it animates a cursor
//!   over its own share while sharing) must land inside the panel's content
//!   area at the same normalized point. The pointer overlay draws at
//!   `x*100% / y*100%` of its own frame, so overlay frame + delivered position
//!   is where the cursor renders.
//! * #316 phantom restore: once the sharer is gone, the ended window must not
//!   be listed by `compositor_list_windows`, and `compositor_activate_window`
//!   on it must be refused (panel stays off screen).

use super::*;
use crate::compositor::{CockpitRemoteChromeSnapshot, CockpitScreenFrame};
use crate::telepointer::CockpitOverlayDelivery;

/// Docking tolerance per edge (points). The dock is exact arithmetic on the
/// panel frame; 2pt absorbs CGWindowList rounding only.
pub(super) const DOCK_TOLERANCE_PT: f64 = 2.0;
/// Rendered-vs-expected telepointer distance budget (points): two docking
/// tolerances composed, rounded up.
pub(super) const POINTER_TOLERANCE_PT: f64 = 3.0;
/// The web peer's sweep is `0.5 + 0.35*sin(..)` on both axes
/// (web-harness `startTelepointerSender`); a delivered point outside this
/// envelope was transformed on the way.
const POINTER_ENVELOPE: (f64, f64) = (0.14, 0.86);
/// A delivery older than this is not "the cursor the overlay shows now" (the
/// sweep publishes every 45ms).
const POINTER_STALE_MS: u64 = 3_000;
/// The product's own dock verifier corrects for up to ~1.5s after a dock
/// pass (10 x 150ms). Only samples past this grace gate; earlier ones are
/// recorded as evidence.
const DOCK_GRACE: Duration = Duration::from_millis(2_000);
const DOCK_GATING_SAMPLES: usize = 4;
const DOCK_SAMPLE_SPACING: Duration = Duration::from_millis(750);
const REVEAL_TIMEOUT: Duration = Duration::from_secs(30);
const REVEAL_POLL: Duration = Duration::from_millis(100);
/// Graceful leave -> TrackUnpublished, or ParticipantDisconnected -> hold ->
/// the 5s reconciliation pass retires it.
const RETIRE_TIMEOUT: Duration = Duration::from_secs(30);
const ACTIVATE_SETTLE: Duration = Duration::from_millis(1_500);

fn off_by_more_than(actual: CockpitScreenFrame, target: CockpitScreenFrame, tol: f64) -> bool {
    (actual.x - target.x).abs() > tol
        || (actual.y - target.y).abs() > tol
        || (actual.width - target.width).abs() > tol
        || (actual.height - target.height).abs() > tol
}

fn at_screen_corner(frame: CockpitScreenFrame) -> bool {
    frame.x.abs() <= DOCK_TOLERANCE_PT
        && (frame.y - STALL_PANEL_HEADER_POINTS).abs() <= DOCK_TOLERANCE_PT
}

fn fmt_frame(frame: CockpitScreenFrame) -> String {
    format!(
        "({:.1},{:.1} {:.1}x{:.1})",
        frame.x, frame.y, frame.width, frame.height
    )
}

/// Everything wrong with one sample's docking; empty = docked.
pub(super) fn dock_problems(snapshot: &CockpitRemoteChromeSnapshot) -> Vec<String> {
    let Some(panel) = snapshot.panel else {
        return vec!["remote panel is not on screen".to_string()];
    };
    let target = crate::compositor::cockpit_chrome_dock_target(panel);
    let mut problems = Vec::new();
    for (name, child) in [("control", snapshot.control), ("pointer", snapshot.pointer)] {
        match child {
            None => problems.push(format!("{name} overlay is not on screen")),
            Some(frame) if off_by_more_than(frame, target, DOCK_TOLERANCE_PT) => {
                let corner = if at_screen_corner(frame) {
                    " -- the #315 screen-corner signature"
                } else {
                    ""
                };
                problems.push(format!(
                    "{name} overlay at {} but panel {} docks it at {}{corner}",
                    fmt_frame(frame),
                    fmt_frame(panel),
                    fmt_frame(target)
                ));
            }
            Some(_) => {}
        }
    }
    problems
}

/// Where the overlay renders the last delivered telepointer vs where that
/// normalized point lies in the panel's content area. `Ok((rendered,
/// expected))` within `POINTER_TOLERANCE_PT`, else the reason.
pub(super) fn pointer_render_check(
    snapshot: &CockpitRemoteChromeSnapshot,
    delivery: Option<CockpitOverlayDelivery>,
    now_ms: u64,
) -> Result<((f64, f64), (f64, f64)), String> {
    let Some(delivery) = delivery else {
        return Err("no telepointer update reached this window's pointer overlay".to_string());
    };
    let age = now_ms.saturating_sub(delivery.at_ms);
    if age > POINTER_STALE_MS {
        return Err(format!(
            "last telepointer update to the pointer overlay is {age}ms old"
        ));
    }
    if !delivery.visible {
        return Err("the delivered telepointer is marked invisible".to_string());
    }
    let (lo, hi) = POINTER_ENVELOPE;
    if !(lo..=hi).contains(&delivery.x) || !(lo..=hi).contains(&delivery.y) {
        return Err(format!(
            "delivered telepointer ({:.3},{:.3}) is outside the sender's sweep envelope [{lo},{hi}]",
            delivery.x, delivery.y
        ));
    }
    let (Some(panel), Some(pointer)) = (snapshot.panel, snapshot.pointer) else {
        return Err("panel or pointer overlay is not on screen".to_string());
    };
    let content = crate::compositor::cockpit_chrome_dock_target(panel);
    let rendered = (
        pointer.x + delivery.x * pointer.width,
        pointer.y + delivery.y * pointer.height,
    );
    let expected = (
        content.x + delivery.x * content.width,
        content.y + delivery.y * content.height,
    );
    let distance = ((rendered.0 - expected.0).powi(2) + (rendered.1 - expected.1).powi(2)).sqrt();
    if distance > POINTER_TOLERANCE_PT {
        return Err(format!(
            "telepointer ({:.3},{:.3}) renders at ({:.1},{:.1}) but belongs at ({:.1},{:.1}) inside the content area -- {distance:.1}pt off",
            delivery.x, delivery.y, rendered.0, rendered.1, expected.0, expected.1
        ));
    }
    Ok((rendered, expected))
}

/// Verdict over the gating samples: every one docked and rendering the
/// pointer in place, and the delivered pointer actually moved between them.
pub(super) fn receiver_window_verdict(
    samples: &[(Vec<String>, Result<((f64, f64), (f64, f64)), String>, Option<(f64, f64)>)],
) -> (Vec<String>, Vec<String>) {
    let mut dock = Vec::new();
    let mut pointer = Vec::new();
    for (index, (dock_problems, pointer_check, _)) in samples.iter().enumerate() {
        dock.extend(dock_problems.iter().map(|p| format!("sample {index}: {p}")));
        if let Err(problem) = pointer_check {
            pointer.push(format!("sample {index}: {problem}"));
        }
    }
    if samples.len() < DOCK_GATING_SAMPLES {
        dock.push(format!(
            "only {} gating sample(s), need {DOCK_GATING_SAMPLES}",
            samples.len()
        ));
    }
    let positions = samples.iter().filter_map(|(_, _, p)| *p).collect::<Vec<_>>();
    let moved = positions.windows(2).any(|pair| {
        (pair[0].0 - pair[1].0).abs() > 0.005 || (pair[0].1 - pair[1].1).abs() > 0.005
    });
    if pointer.is_empty() && !moved {
        pointer.push(format!(
            "the delivered telepointer never moved across {} samples ({positions:?})",
            samples.len()
        ));
    }
    (dock, pointer)
}

fn snapshot_for(app: &AppHandle, owner: &str, window_id: Option<u32>) -> Option<CockpitRemoteChromeSnapshot> {
    crate::compositor::cockpit_remote_chrome_snapshots(app, owner)
        .ok()?
        .into_iter()
        .find(|snapshot| match window_id {
            Some(id) => snapshot.source_window_id == id,
            None => snapshot.panel.is_some(),
        })
}

/// SHARE-W2N-Q's verdict: the existing video oracle, then the #315 docking +
/// pointer and #316 phantom-restore receiver checks.
pub(super) async fn assert_web_to_native_receiver(
    app: &AppHandle,
    scenario: ScenarioSpec,
    report: &WebCockpitReport,
    writer: &mut ResultsWriter,
) -> ScenarioOutcome {
    let owner = report.sender.clone();
    let reveal_wait = Instant::now();
    let mut revealed: Option<(Instant, CockpitRemoteChromeSnapshot)> = None;
    while reveal_wait.elapsed() < REVEAL_TIMEOUT {
        if let Some(snapshot) = snapshot_for(app, &owner, None) {
            revealed = Some((Instant::now(), snapshot));
            break;
        }
        tokio::time::sleep(REVEAL_POLL).await;
    }
    if let Some((_, snapshot)) = revealed.as_ref() {
        // Evidence only: the product's verifier may still be correcting.
        let _ = writer.write(
            "receiver-window-sample",
            Some(scenario.id),
            serde_json::json!({
                "phase": "reveal",
                "gating": false,
                "snapshot": snapshot,
                "dockProblems": dock_problems(snapshot),
            }),
        );
    }

    let mut outcome = assert_web_to_native_video(app, scenario, writer).await;
    if outcome.verdict != ScenarioVerdict::Pass {
        return outcome;
    }
    let Some((revealed_at, first)) = revealed else {
        return fail(
            outcome,
            scenario,
            "receiver-window-docked",
            format!("video arrived but no remote panel for '{owner}' came on screen within {REVEAL_TIMEOUT:?}"),
        );
    };
    let window_id = first.source_window_id;

    // #315 docking + pointer placement, past the verifier's grace.
    let since_reveal = revealed_at.elapsed();
    if since_reveal < DOCK_GRACE {
        tokio::time::sleep(DOCK_GRACE - since_reveal).await;
    }
    // RED-PROOF ONLY.
    crate::compositor::cockpit_red_inject_corner_misdock(app, &owner);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut samples = Vec::new();
    let mut last_panel_cg_id = first.panel_cg_window_id;
    for index in 0..DOCK_GATING_SAMPLES {
        if index > 0 {
            tokio::time::sleep(DOCK_SAMPLE_SPACING).await;
        }
        let snapshot = snapshot_for(app, &owner, Some(window_id));
        let (problems, pointer_check, position) = match snapshot.as_ref() {
            Some(snapshot) => {
                last_panel_cg_id = snapshot.panel_cg_window_id.or(last_panel_cg_id);
                let delivery =
                    crate::telepointer::cockpit_last_remote_overlay_delivery(&snapshot.pointer_label);
                (
                    dock_problems(snapshot),
                    pointer_render_check(snapshot, delivery, crate::time_util::now_ms()),
                    delivery.map(|d| (d.x, d.y)),
                )
            }
            None => (
                vec![format!("remote window {window_id} is no longer open")],
                Err("remote window is no longer open".to_string()),
                None,
            ),
        };
        let _ = writer.write(
            "receiver-window-sample",
            Some(scenario.id),
            serde_json::json!({
                "phase": "steady",
                "gating": true,
                "index": index,
                "msSinceReveal": revealed_at.elapsed().as_millis() as u64,
                "snapshot": snapshot,
                "dockProblems": problems,
                "pointer": match &pointer_check {
                    Ok((rendered, expected)) => serde_json::json!({ "rendered": rendered, "expected": expected, "position": position }),
                    Err(problem) => serde_json::json!({ "problem": problem, "position": position }),
                },
            }),
        );
        samples.push((problems, pointer_check, position));
    }
    let (dock, pointer) = receiver_window_verdict(&samples);
    outcome.assertions.push(AssertionOutcome {
        name: "receiver-window-docked".to_string(),
        passed: dock.is_empty(),
        detail: if dock.is_empty() {
            format!(
                "control+pointer overlays docked within {DOCK_TOLERANCE_PT}pt in {} WindowServer samples (#315)",
                samples.len()
            )
        } else {
            dock.join("; ")
        },
    });
    outcome.assertions.push(AssertionOutcome {
        name: "receiver-telepointer-placement".to_string(),
        passed: pointer.is_empty(),
        detail: if pointer.is_empty() {
            format!(
                "sharer telepointer rendered within {POINTER_TOLERANCE_PT}pt of its content-area point in {} samples (#315)",
                samples.len()
            )
        } else {
            pointer.join("; ")
        },
    });

    // #316: end the share, then try to bring it back.
    let phantom = check_ended_window_not_restorable(app, &owner, window_id, last_panel_cg_id).await;
    let _ = writer.write(
        "receiver-window-ended",
        Some(scenario.id),
        serde_json::json!({ "windowId": window_id, "panelCgWindowId": last_panel_cg_id, "result": &phantom }),
    );
    outcome.assertions.push(AssertionOutcome {
        name: "ended-window-not-restorable".to_string(),
        passed: phantom.is_ok(),
        detail: match &phantom {
            Ok(detail) => detail.clone(),
            Err(problem) => problem.clone(),
        },
    });

    let failed = outcome
        .assertions
        .iter()
        .filter(|assertion| !assertion.passed && assertion.name != "recv-window-active-fps")
        .map(|assertion| assertion.name.clone())
        .collect::<Vec<_>>();
    if !failed.is_empty() {
        outcome.verdict = ScenarioVerdict::TestFail;
        outcome.message = format!(
            "{} TEST-FAIL video arrived but the receiver window failed {failed:?}",
            scenario.id
        );
    }
    outcome
}

fn fail(
    mut outcome: ScenarioOutcome,
    scenario: ScenarioSpec,
    name: &str,
    detail: String,
) -> ScenarioOutcome {
    outcome.verdict = ScenarioVerdict::TestFail;
    outcome.message = format!("{} TEST-FAIL {detail}", scenario.id);
    outcome.assertions.push(AssertionOutcome {
        name: name.to_string(),
        passed: false,
        detail,
    });
    outcome
}

/// #316: disconnect the sharer, wait for its window to be retired, then
/// assert the menubar list omits it and an activate leaves it off screen.
async fn check_ended_window_not_restorable(
    app: &AppHandle,
    owner: &str,
    window_id: u32,
    panel_cg_window_id: Option<u32>,
) -> Result<String, String> {
    publish_cockpit_command(app, owner, "disconnect").await?;
    let started = Instant::now();
    while crate::compositor::is_open_for_owner(owner, window_id) {
        if started.elapsed() > RETIRE_TIMEOUT {
            return Err(format!(
                "the sharer left but its window {window_id} was still open after {RETIRE_TIMEOUT:?}"
            ));
        }
        tokio::time::sleep(REVEAL_POLL).await;
    }
    // The park into the retired pool is a main-thread hop after the removal.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let retired = crate::compositor::retired_window_ids_for_participant(owner).contains(&window_id);
    let retire_ms = started.elapsed().as_millis();
    // Read the list as the menubar receives it (the command's wire shape).
    let listed = crate::compositor::compositor_list_windows()
        .into_iter()
        .filter_map(|entry| serde_json::to_value(entry).ok())
        .find(|entry| {
            entry["ownerIdentity"].as_str() == Some(owner)
                && entry["windowId"].as_u64() == Some(u64::from(window_id))
        });
    if let Some(entry) = listed {
        return Err(format!(
            "ended window {window_id} is still listed by compositor_list_windows (hidden={}) -- the #316 phantom",
            entry["hidden"]
        ));
    }
    crate::compositor::compositor_activate_window(app.clone(), window_id, Some(owner.to_string()));
    tokio::time::sleep(ACTIVATE_SETTLE).await;
    let reopened = crate::compositor::is_open_for_owner(owner, window_id);
    let on_screen = panel_cg_window_id
        .and_then(crate::platform::cg::frame_for_window_id_raw)
        .is_some();
    if reopened || on_screen {
        return Err(format!(
            "compositor_activate_window restored ended window {window_id} (open={reopened}, panel on screen={on_screen}) -- the #316 phantom restore"
        ));
    }
    Ok(format!(
        "window {window_id} closed {retire_ms}ms after the sharer left (retired={retired}); not listed, activate refused, panel stays off screen (#316)"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(x: f64, y: f64, width: f64, height: f64) -> CockpitScreenFrame {
        CockpitScreenFrame {
            x,
            y,
            width,
            height,
        }
    }

    /// Panel at the field position; children docked under its 44pt header.
    fn docked_snapshot() -> CockpitRemoteChromeSnapshot {
        CockpitRemoteChromeSnapshot {
            source_window_id: 38068,
            panel_cg_window_id: Some(9001),
            panel: Some(frame(217.0, 175.0, 960.0, 644.0)),
            control: Some(frame(217.0, 219.0, 960.0, 600.0)),
            pointer: Some(frame(217.0, 219.0, 960.0, 600.0)),
            pointer_label: "remote-window-pointer-x".to_string(),
        }
    }

    fn delivery(x: f64, y: f64, at_ms: u64) -> Option<CockpitOverlayDelivery> {
        Some(CockpitOverlayDelivery {
            x,
            y,
            visible: true,
            at_ms,
        })
    }

    #[test]
    fn issue_315_docked_children_pass() {
        assert!(dock_problems(&docked_snapshot()).is_empty());
        // CGWindowList rounding stays inside the tolerance.
        let mut snapshot = docked_snapshot();
        snapshot.pointer = Some(frame(218.5, 219.0, 960.0, 600.0));
        assert!(dock_problems(&snapshot).is_empty());
    }

    #[test]
    fn issue_315_field_signature_children_at_screen_corner_fail() {
        let mut snapshot = docked_snapshot();
        snapshot.control = Some(frame(0.0, 44.0, 960.0, 600.0));
        snapshot.pointer = Some(frame(0.0, 44.0, 960.0, 600.0));
        let problems = dock_problems(&snapshot);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems.iter().all(|p| p.contains("#315 screen-corner signature")), "{problems:?}");
    }

    #[test]
    fn issue_315_small_misdock_and_missing_child_fail() {
        let mut snapshot = docked_snapshot();
        snapshot.control = Some(frame(217.0, 222.5, 960.0, 600.0));
        snapshot.pointer = None;
        let problems = dock_problems(&snapshot);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].contains("control overlay at") && !problems[0].contains("corner"));
        assert!(problems[1].contains("pointer overlay is not on screen"));
        let mut hidden = docked_snapshot();
        hidden.panel = None;
        assert_eq!(dock_problems(&hidden), vec!["remote panel is not on screen"]);
    }

    #[test]
    fn issue_315_pointer_renders_at_its_content_point_when_docked() {
        let (rendered, expected) =
            pointer_render_check(&docked_snapshot(), delivery(0.25, 0.25, 1_000), 1_100).unwrap();
        assert_eq!(rendered, (217.0 + 240.0, 219.0 + 150.0));
        assert_eq!(rendered, expected);
        assert!(pointer_render_check(&docked_snapshot(), delivery(0.75, 0.6, 1_000), 1_100).is_ok());
    }

    #[test]
    fn issue_315_pointer_offset_by_corner_misdock_fails() {
        let mut snapshot = docked_snapshot();
        snapshot.pointer = Some(frame(0.0, 44.0, 960.0, 600.0));
        let error = pointer_render_check(&snapshot, delivery(0.25, 0.25, 1_000), 1_100).unwrap_err();
        assert!(error.contains("renders at (240.0,194.0) but belongs at (457.0,369.0)"), "{error}");
    }

    #[test]
    fn pointer_check_refuses_missing_stale_hidden_or_transformed_deliveries() {
        let snapshot = docked_snapshot();
        assert!(pointer_render_check(&snapshot, None, 0).unwrap_err().contains("no telepointer"));
        assert!(pointer_render_check(&snapshot, delivery(0.5, 0.5, 1_000), 5_000)
            .unwrap_err()
            .contains("4000ms old"));
        let hidden = Some(CockpitOverlayDelivery {
            visible: false,
            ..delivery(0.5, 0.5, 1_000).unwrap()
        });
        assert!(pointer_render_check(&snapshot, hidden, 1_000).unwrap_err().contains("invisible"));
        // Pixels instead of normalized coordinates.
        assert!(pointer_render_check(&snapshot, delivery(480.0, 300.0, 1_000), 1_000)
            .unwrap_err()
            .contains("envelope"));
    }

    #[test]
    fn verdict_requires_enough_docked_samples_and_a_moving_pointer() {
        let ok = |p: (f64, f64)| (Vec::new(), Ok(((0.0, 0.0), (0.0, 0.0))), Some(p));
        let moving = vec![ok((0.3, 0.3)), ok((0.4, 0.35)), ok((0.5, 0.4)), ok((0.6, 0.5))];
        assert_eq!(receiver_window_verdict(&moving), (vec![], vec![]));

        let frozen = vec![ok((0.3, 0.3)); 4];
        let (dock, pointer) = receiver_window_verdict(&frozen);
        assert!(dock.is_empty());
        assert!(pointer[0].contains("never moved"), "{pointer:?}");

        let (dock, _) = receiver_window_verdict(&moving[..2]);
        assert!(dock[0].contains("only 2 gating sample"), "{dock:?}");

        let mut misdocked = moving.clone();
        misdocked[3].0 = vec!["pointer overlay at (0.0,44.0 960.0x600.0)".to_string()];
        let (dock, _) = receiver_window_verdict(&misdocked);
        assert_eq!(dock, vec!["sample 3: pointer overlay at (0.0,44.0 960.0x600.0)"]);
    }
}
