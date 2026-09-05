//! Acting on the shared window, and everything re-checked at the moment of
//! acting (#658 phase 3).
//!
//! ## The core idea: an approval is not a licence
//!
//! Time passes between the model asking, the human answering, and the action
//! running. In that gap the share can stop, the window can move or close, the
//! user can switch to their password manager, a secure field can take the
//! keyboard, and the digest the model cited can go stale. So **nothing decided
//! earlier is trusted here**: [`recheck`] re-reads the whole world and runs it
//! back through `control_policy`'s untouched matrix, and the per-action helpers
//! re-resolve their target against current state.
//!
//! ## Reusing the existing input stack
//!
//! Execution goes through `remote_control.rs`'s `input::replay` — the same
//! battle-tested accessibility-first replay human remote control uses — rather
//! than a second AX/CGEvent stack. That matters beyond duplication: crash class
//! 4 in CLAUDE.md records that `CGEventPostToPid` does nothing at all for
//! pointer and scroll, so a fresh implementation would very likely have
//! reinvented a silent no-op. `replay` already routes pointer and scroll
//! through the accessibility path and falls back only where that is meaningful.
//!
//! ## What the codes mean
//!
//! Every refusal is a stable token echoed to the model in the tool response and
//! written to the room transcript. They are deliberately specific: "the window
//! stopped being shared" and "a password field has the keyboard" should not
//! look the same to a user reading the audit trail.

use crate::platform::cg::WindowFrame;
use crate::remote_control_core::{
    RemoteControlAction, RemoteControlMessage, RemoteControlModifiers, RemoteControlType, VERSION,
};

use super::ax_digest::{focused_typing_refusal, DigestRect, FocusedContext};
use super::control_gate::Authorization;
use super::control_policy::{grant_decision, Action, Decision, GrantContext, SecureInput};

/// How long we will wait for the target application to actually come to the
/// front. Bounded and polled — never a blind sleep, which would either be too
/// short to be reliable or long enough to act on a window the user has since
/// moved away from.
pub const FRONTMOST_WAIT_MS: u64 = 500;
/// Poll interval inside that window.
pub const FRONTMOST_POLL_MS: u64 = 25;

/// The world as re-read immediately before acting.
#[derive(Debug, Clone)]
pub struct ExecContext<'a> {
    /// `PETAL_AI_CONTROL=1`.
    pub control_enabled: bool,
    pub authorization: Authorization,
    pub ai_chat_enabled: bool,
    pub remote_control_allowed: bool,
    /// The #656 publication gate, re-evaluated now: is this window still a live
    /// publication owned by this client?
    pub publication_live: bool,
    /// The window's CURRENT frame. `None` means it has gone.
    pub window_frame: Option<WindowFrame>,
    pub target_pid: Option<i32>,
    /// The TARGET application's bundle id — not merely whatever is frontmost —
    /// so the blocklist is applied to the thing we are about to drive.
    pub target_bundle_id: Option<&'a str>,
    pub secure_input: SecureInput,
    pub takeover_healthy: bool,
    /// Result of the bounded frontmost poll.
    pub target_is_frontmost: bool,
}

/// Outcome of resolving a model-cited `[n]` against the generation it named.
#[derive(Debug, Clone, PartialEq)]
pub enum ClickResolution {
    Resolved(DigestRect),
    /// The cited generation has fallen out of history, or the index was not in
    /// it. The model must be told AND given a fresh digest — otherwise it
    /// retries the same dead reference.
    StaleGeneration,
    /// The element resolved but accessibility never gave it a usable position,
    /// so there is no point we could prove is inside the window.
    Unpositioned,
}

/// Re-run every gate at the moment of execution.
///
/// The policy matrix itself is NOT re-implemented here: the shared checks are
/// delegated to `control_policy::grant_decision` so there is exactly one
/// fail-closed matrix in the codebase. What this adds is the set of facts that
/// only exist at execution time — authorization standing, the live publication,
/// the current frame, and whether the target is actually in front.
pub fn recheck(action: &Action, ctx: &ExecContext<'_>) -> Result<(), &'static str> {
    // The master switch first: with control disarmed nothing below matters, and
    // reading it here (not only where tools are declared) means an execution
    // path reached by any future caller is still dark.
    if !ctx.control_enabled {
        return Err("control_disabled");
    }
    match ctx.authorization {
        Authorization::Refused => return Err("control_rejected"),
        Authorization::NotGranted => return Err("control_not_granted"),
        Authorization::Once | Authorization::Session => {}
    }
    // The share may have stopped between approval and execution. This is the
    // same gate `session.rs` re-checks before every capture; an action outlives
    // a capture, so it has to be asked again here.
    if !ctx.publication_live {
        return Err("window_not_shared");
    }
    let Some(_frame) = ctx.window_frame else {
        return Err("window_unavailable");
    };
    if ctx.target_pid.filter(|pid| *pid > 0).is_none() {
        return Err("target_unavailable");
    }
    let grant = GrantContext {
        window_present: true,
        bundle_id: ctx.target_bundle_id,
        secure_input: ctx.secure_input,
        takeover_detection_healthy: ctx.takeover_healthy,
        remote_control_allowed: ctx.remote_control_allowed,
        ai_chat_enabled: ctx.ai_chat_enabled,
    };
    if let Decision::Refuse { code } = grant_decision(action, &grant) {
        return Err(code);
    }
    // Last: the app we are about to drive must actually own the foreground.
    // Injected input lands on the focused application, so acting while
    // something else is in front would deliver the action to the wrong app.
    if !ctx.target_is_frontmost {
        return Err("target_not_frontmost");
    }
    Ok(())
}

/// Turn a cited element into a point, or refuse.
///
/// Both failure modes are real and distinct: a stale generation means the model
/// is working from a snapshot that no longer exists, and an out-of-frame point
/// means the element has moved (or the window has) since that snapshot. Neither
/// may fall back to "click roughly there".
pub fn plan_click(
    resolution: &ClickResolution,
    frame: WindowFrame,
) -> Result<(f64, f64), &'static str> {
    let rect = match resolution {
        ClickResolution::StaleGeneration => return Err("stale_digest_generation"),
        ClickResolution::Unpositioned => return Err("element_unpositioned"),
        ClickResolution::Resolved(rect) => rect,
    };
    if !rect.is_usable() {
        return Err("element_unpositioned");
    }
    let (x, y) = rect.center();
    if !point_in_frame(x, y, frame) {
        return Err("element_out_of_frame");
    }
    Ok((x, y))
}

/// Is this global point inside the window's current frame? Half-open on the far
/// edges: a point exactly on the right or bottom edge belongs to whatever is
/// next to the window, not to it.
pub fn point_in_frame(x: f64, y: f64, frame: WindowFrame) -> bool {
    let left = frame.x as f64;
    let top = frame.y as f64;
    let right = left + frame.width.max(0) as f64;
    let bottom = top + frame.height.max(0) as f64;
    x >= left && x < right && y >= top && y < bottom
}

/// Whether typing may proceed into whatever the target app currently focuses.
/// Delegates to the digest's fail-closed rule so "secure" is recognised by the
/// same predicate that keeps secure fields out of the digest itself.
pub fn plan_type(focused: &FocusedContext) -> Result<(), &'static str> {
    match focused_typing_refusal(focused) {
        Some(code) => Err(code),
        None => Ok(()),
    }
}

/// Normalize a global point into the window-relative fraction `input::replay`
/// expects. Clamped, because a point that survived [`point_in_frame`] is inside
/// the frame and a rounding excursion must not become an off-window click.
pub fn normalized_in_frame(x: f64, y: f64, frame: WindowFrame) -> (f64, f64) {
    let width = frame.width.max(1) as f64;
    let height = frame.height.max(1) as f64;
    (
        ((x - frame.x as f64) / width).clamp(0.0, 1.0),
        ((y - frame.y as f64) / height).clamp(0.0, 1.0),
    )
}

/// Refuse text the replay layer would silently shorten.
///
/// `control_policy` admits up to 2000 characters, but `remote_control`'s text
/// replay truncates at `MAX_REPLAY_TEXT_CHARS`. A human who approved a specific
/// string must get that string or nothing — typing the first 1000 characters of
/// an approved 1500-character message is a different action than the one on the
/// card, and half a sentence can change its meaning entirely.
pub fn plan_text(text: &str) -> Result<(), &'static str> {
    if text.chars().count() > crate::remote_control::MAX_REPLAY_TEXT_CHARS {
        return Err("text_exceeds_replay_limit");
    }
    Ok(())
}

// ---- dispatch ---------------------------------------------------------------

/// Identity stamped on the synthesized input packets. It is never sent over the
/// wire — the messages are handed straight to the local replay layer — but
/// `remote_control` keys per-controller gesture state on it, so it must be
/// distinct from any real participant.
pub const CONTROLLER_ID: &str = "petal-ai-chat";

fn next_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(1);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

fn base_message(window_id: u32, kind: RemoteControlType) -> RemoteControlMessage {
    RemoteControlMessage {
        v: VERSION,
        message_type: kind,
        action: None,
        target_user_id: CONTROLLER_ID.to_string(),
        controller_id: CONTROLLER_ID.to_string(),
        window_id,
        seq: next_seq(),
        target_kind: None,
        share_instance_id: None,
        controller_capabilities: Vec::new(),
        host_capabilities: Vec::new(),
        reason: None,
        control_session_id: None,
        input_id: None,
        input_seq: None,
        operation_fingerprint_version: None,
        operation_fingerprint: None,
        outcome: None,
        delivery_route: None,
        failure_code: None,
        result_capability: None,
        x: None,
        y: None,
        button: None,
        buttons: None,
        click_count: None,
        delta_x: None,
        delta_y: None,
        delta_mode: None,
        key: None,
        code: None,
        repeat: false,
        location: None,
        text: None,
        status: None,
        message: None,
        grant_token: None,
        supports_binary_hot_path: false,
        modifiers: RemoteControlModifiers::default(),
    }
}

/// DOM-style `code`/`key` names for the enumerated navigation keys, which is
/// the vocabulary `remote_control`'s keycode table already speaks.
pub fn key_code_name(key: super::control_policy::SafeKey) -> &'static str {
    use super::control_policy::SafeKey;
    match key {
        SafeKey::Return => "Enter",
        SafeKey::Tab => "Tab",
        SafeKey::Escape => "Escape",
        SafeKey::ArrowUp => "ArrowUp",
        SafeKey::ArrowDown => "ArrowDown",
        SafeKey::ArrowLeft => "ArrowLeft",
        SafeKey::ArrowRight => "ArrowRight",
    }
}

/// Line deltas for a scroll, in the DOM's sign convention (`deltaY > 0` scrolls
/// content down), which is what the replay layer's `delta_mode: 1` path
/// expects.
pub fn scroll_deltas(direction: super::control_policy::ScrollDirection, amount: i64) -> (f64, f64) {
    use super::control_policy::ScrollDirection;
    let amount = amount as f64;
    match direction {
        ScrollDirection::Up => (0.0, -amount),
        ScrollDirection::Down => (0.0, amount),
        ScrollDirection::Left => (-amount, 0.0),
        ScrollDirection::Right => (amount, 0.0),
    }
}

/// Hand the action to `remote_control`'s replay.
///
/// `point` is the already-validated global click point; every other action acts
/// at the window's centre (scroll) or on the focused element (type, key), which
/// the caller has separately verified.
pub fn dispatch(
    action: &Action,
    window_id: u32,
    frame: WindowFrame,
    pid: i32,
    point: Option<(f64, f64)>,
) -> Result<(), String> {
    use crate::remote_control::input;

    match action {
        Action::Type(text) => {
            let mut message = base_message(window_id, RemoteControlType::Text);
            message.text = Some(text.clone());
            input::replay(&message, frame, Some(pid))
        }
        Action::Click { .. } => {
            let (x, y) = point.ok_or_else(|| "click dispatched without a point".to_string())?;
            let (nx, ny) = normalized_in_frame(x, y, frame);
            let mut message = base_message(window_id, RemoteControlType::Pointer);
            // A complete, non-dragging click: it leaves no held-button gesture
            // state behind, so an interrupted action cannot strand the target
            // app with the mouse down.
            message.action = Some(RemoteControlAction::Click);
            message.x = Some(nx);
            message.y = Some(ny);
            message.button = Some(0);
            message.click_count = Some(1);
            input::replay(&message, frame, Some(pid))
        }
        Action::PressKey(key) => {
            let name = key_code_name(*key);
            for press in [RemoteControlAction::Down, RemoteControlAction::Up] {
                let mut message = base_message(window_id, RemoteControlType::Key);
                message.action = Some(press);
                message.code = Some(name.to_string());
                message.key = Some(name.to_string());
                input::replay(&message, frame, Some(pid))?;
            }
            Ok(())
        }
        Action::Scroll { direction, amount } => {
            let (delta_x, delta_y) = scroll_deltas(*direction, *amount);
            let mut message = base_message(window_id, RemoteControlType::Wheel);
            // Centre of the window: the replay layer hit-tests this point to
            // find the scrollable element.
            message.x = Some(0.5);
            message.y = Some(0.5);
            message.delta_x = Some(delta_x);
            message.delta_y = Some(delta_y);
            message.delta_mode = Some(1); // lines
            input::replay(&message, frame, Some(pid))
        }
    }
}

/// Wait, bounded and polled, for `target_pid` to own the foreground.
///
/// Never a blind sleep: a fixed delay is either too short to be reliable or
/// long enough that the window we checked is no longer the one in front.
pub fn wait_for_frontmost(target_pid: i32) -> bool {
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_millis(FRONTMOST_WAIT_MS);
    loop {
        if super::control_target::frontmost_app().is_some_and(|app| app.pid == target_pid) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(FRONTMOST_POLL_MS));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai_chat::control_policy::{SafeKey, ScrollDirection};

    fn frame() -> WindowFrame {
        WindowFrame {
            x: 100,
            y: 200,
            width: 400,
            height: 300,
        }
    }

    fn ok_ctx() -> ExecContext<'static> {
        ExecContext {
            control_enabled: true,
            authorization: Authorization::Once,
            ai_chat_enabled: true,
            remote_control_allowed: true,
            publication_live: true,
            window_frame: Some(frame()),
            target_pid: Some(4242),
            target_bundle_id: Some("com.apple.TextEdit"),
            secure_input: SecureInput::Inactive,
            takeover_healthy: true,
            target_is_frontmost: true,
        }
    }

    fn typing() -> Action {
        Action::Type("hello".into())
    }

    fn clicking() -> Action {
        Action::Click {
            generation: 3,
            element_index: 1,
        }
    }

    #[test]
    fn a_fully_clean_context_is_allowed() {
        assert_eq!(recheck(&typing(), &ok_ctx()), Ok(()));
        assert_eq!(recheck(&clicking(), &ok_ctx()), Ok(()));
    }

    #[test]
    fn the_master_switch_being_off_refuses_before_anything_else() {
        let ctx = ExecContext {
            control_enabled: false,
            ..ok_ctx()
        };
        assert_eq!(recheck(&typing(), &ctx), Err("control_disabled"));
        // Even with a session-wide grant already standing.
        let ctx = ExecContext {
            control_enabled: false,
            authorization: Authorization::Session,
            ..ok_ctx()
        };
        assert_eq!(recheck(&typing(), &ctx), Err("control_disabled"));
    }

    #[test]
    fn an_ungranted_or_revoked_action_never_runs() {
        for (authorization, code) in [
            (Authorization::NotGranted, "control_not_granted"),
            (Authorization::Refused, "control_rejected"),
        ] {
            let ctx = ExecContext {
                authorization,
                ..ok_ctx()
            };
            assert_eq!(recheck(&typing(), &ctx), Err(code), "{authorization:?}");
        }
    }

    #[test]
    fn losing_the_publication_mid_action_refuses() {
        // The human approved while the window was shared; the share stopped
        // before the action ran. #656's gate has to hold here too.
        let ctx = ExecContext {
            publication_live: false,
            ..ok_ctx()
        };
        assert_eq!(recheck(&typing(), &ctx), Err("window_not_shared"));
    }

    #[test]
    fn a_window_that_vanished_or_lost_its_process_refuses() {
        let ctx = ExecContext {
            window_frame: None,
            ..ok_ctx()
        };
        assert_eq!(recheck(&typing(), &ctx), Err("window_unavailable"));

        for pid in [None, Some(0), Some(-1)] {
            let ctx = ExecContext {
                target_pid: pid,
                ..ok_ctx()
            };
            assert_eq!(recheck(&typing(), &ctx), Err("target_unavailable"), "{pid:?}");
        }
    }

    #[test]
    fn a_blocklisted_target_refuses_even_after_approval() {
        // Including an IDE: an integrated terminal reaches a shell exactly as
        // Terminal.app does, and the human's yes cannot make that safe.
        for (bundle, code) in [
            ("com.microsoft.VSCode", "blocked_editor"),
            ("com.todesktop.230313mzl4w4u92", "blocked_editor"),
            ("com.apple.dt.Xcode", "blocked_editor"),
            ("com.apple.Terminal", "blocked_terminal"),
            ("com.1password.1password", "blocked_password_manager"),
            ("com.petal.app", "blocked_self"),
        ] {
            let ctx = ExecContext {
                target_bundle_id: Some(bundle),
                ..ok_ctx()
            };
            assert_eq!(recheck(&typing(), &ctx), Err(code), "{bundle}");
        }
        // An app we cannot identify is refused too.
        let ctx = ExecContext {
            target_bundle_id: None,
            ..ok_ctx()
        };
        assert_eq!(
            recheck(&typing(), &ctx),
            Err("unknown_target_application")
        );
    }

    #[test]
    fn secure_input_active_and_unknown_both_refuse() {
        for state in [SecureInput::Active, SecureInput::Unknown] {
            let ctx = ExecContext {
                secure_input: state,
                ..ok_ctx()
            };
            assert_eq!(
                recheck(&typing(), &ctx),
                Err("secure_input_active"),
                "{state:?}"
            );
        }
    }

    #[test]
    fn an_unavailable_detector_refuses_the_higher_tier_only() {
        let ctx = ExecContext {
            takeover_healthy: false,
            ..ok_ctx()
        };
        for action in [
            clicking(),
            Action::PressKey(SafeKey::Return),
            Action::Scroll {
                direction: ScrollDirection::Down,
                amount: 3,
            },
        ] {
            assert_eq!(
                recheck(&action, &ctx),
                Err("input_tap_unavailable"),
                "{action:?}"
            );
        }
        // Typing into an already-verified focused field stays available.
        assert_eq!(recheck(&typing(), &ctx), Ok(()));
    }

    #[test]
    fn acting_while_another_app_is_in_front_refuses() {
        let ctx = ExecContext {
            target_is_frontmost: false,
            ..ok_ctx()
        };
        assert_eq!(recheck(&typing(), &ctx), Err("target_not_frontmost"));
    }

    #[test]
    fn a_stale_digest_generation_is_refused_rather_than_resolved() {
        // The whole reason a click cites its generation: without this the index
        // silently resolves to whatever now sits in that slot.
        assert_eq!(
            plan_click(&ClickResolution::StaleGeneration, frame()),
            Err("stale_digest_generation")
        );
    }

    #[test]
    fn an_element_outside_the_current_frame_is_refused() {
        // The window moved (or the element did) since the snapshot.
        let outside = DigestRect {
            x: 900.0,
            y: 900.0,
            width: 40.0,
            height: 20.0,
        };
        assert_eq!(
            plan_click(&ClickResolution::Resolved(outside), frame()),
            Err("element_out_of_frame")
        );

        // Just past the far edge counts as outside.
        let edge = DigestRect {
            x: 480.0,
            y: 400.0,
            width: 40.0,
            height: 20.0,
        };
        assert_eq!(edge.center().0, 500.0);
        assert_eq!(
            plan_click(&ClickResolution::Resolved(edge), frame()),
            Err("element_out_of_frame")
        );
    }

    #[test]
    fn an_unpositioned_or_degenerate_element_is_refused() {
        assert_eq!(
            plan_click(&ClickResolution::Unpositioned, frame()),
            Err("element_unpositioned")
        );
        for rect in [
            DigestRect {
                x: 150.0,
                y: 250.0,
                width: 0.0,
                height: 20.0,
            },
            DigestRect {
                x: f64::NAN,
                y: 250.0,
                width: 10.0,
                height: 20.0,
            },
        ] {
            assert_eq!(
                plan_click(&ClickResolution::Resolved(rect), frame()),
                Err("element_unpositioned"),
                "{rect:?}"
            );
        }
    }

    #[test]
    fn a_resolved_in_frame_element_yields_its_centre() {
        let rect = DigestRect {
            x: 150.0,
            y: 250.0,
            width: 40.0,
            height: 20.0,
        };
        assert_eq!(
            plan_click(&ClickResolution::Resolved(rect), frame()),
            Ok((170.0, 260.0))
        );
        // ... and normalizes into the frame the replay layer expects.
        let (nx, ny) = normalized_in_frame(170.0, 260.0, frame());
        assert!((nx - 0.175).abs() < 1e-9, "{nx}");
        assert!((ny - 0.2).abs() < 1e-9, "{ny}");
    }

    #[test]
    fn typing_refuses_an_unknown_or_secure_focused_element() {
        // No role at all: we cannot prove it is not a password field.
        assert_eq!(
            plan_type(&FocusedContext {
                role: None,
                subrole: None,
                window_matches: true,
            }),
            Err("focused_role_unknown")
        );
        assert_eq!(
            plan_type(&FocusedContext {
                role: Some("   ".into()),
                subrole: None,
                window_matches: true,
            }),
            Err("focused_role_unknown")
        );
        // A secure field, by role or by subrole.
        assert_eq!(
            plan_type(&FocusedContext {
                role: Some("AXSecureTextField".into()),
                subrole: None,
                window_matches: true,
            }),
            Err("focused_field_secure")
        );
        assert_eq!(
            plan_type(&FocusedContext {
                role: Some("AXTextField".into()),
                subrole: Some("AXSecureTextField".into()),
                window_matches: true,
            }),
            Err("focused_field_secure")
        );
        // Focus is in a different window of the same app.
        assert_eq!(
            plan_type(&FocusedContext {
                role: Some("AXTextField".into()),
                subrole: None,
                window_matches: false,
            }),
            Err("focused_window_mismatch")
        );
        // The one accepted shape.
        assert_eq!(
            plan_type(&FocusedContext {
                role: Some("AXTextArea".into()),
                subrole: None,
                window_matches: true,
            }),
            Ok(())
        );
    }

    #[test]
    fn text_the_replay_layer_would_shorten_is_refused_not_truncated() {
        let limit = crate::remote_control::MAX_REPLAY_TEXT_CHARS;
        assert_eq!(plan_text(&"a".repeat(limit)), Ok(()));
        assert_eq!(
            plan_text(&"a".repeat(limit + 1)),
            Err("text_exceeds_replay_limit")
        );
        // Counted in characters, not bytes, so multi-byte text is not refused
        // early — and equally not truncated mid-grapheme.
        assert_eq!(plan_text(&"é".repeat(limit)), Ok(()));
    }

    #[test]
    fn keys_and_scrolls_map_to_the_replay_layers_vocabulary() {
        assert_eq!(key_code_name(SafeKey::Return), "Enter");
        assert_eq!(key_code_name(SafeKey::Escape), "Escape");
        assert_eq!(key_code_name(SafeKey::ArrowLeft), "ArrowLeft");
        assert_eq!(scroll_deltas(ScrollDirection::Down, 3), (0.0, 3.0));
        assert_eq!(scroll_deltas(ScrollDirection::Up, 3), (0.0, -3.0));
        assert_eq!(scroll_deltas(ScrollDirection::Right, 2), (2.0, 0.0));
        assert_eq!(scroll_deltas(ScrollDirection::Left, 2), (-2.0, 0.0));
    }

    #[test]
    fn frame_containment_is_half_open_on_the_far_edges() {
        let f = frame();
        assert!(point_in_frame(100.0, 200.0, f)); // top-left corner is inside
        assert!(!point_in_frame(500.0, 300.0, f)); // right edge is not
        assert!(!point_in_frame(300.0, 500.0, f)); // bottom edge is not
        assert!(!point_in_frame(99.0, 300.0, f));
    }
}
