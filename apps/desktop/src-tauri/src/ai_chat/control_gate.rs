//! Who authorized what, and for exactly how long (#658 phase 3).
//!
//! `control_policy.rs` decides whether an action may even be *offered* to a
//! human. This module owns what happens after that: the pending request, the
//! human's answer, and the standing authorization that answer creates. It is
//! deliberately pure — no AppKit, no sockets — so every one of its fail-closed
//! properties is unit-testable.
//!
//! ## Three rules that the tests below pin
//!
//! 1. **"Allow once" authorizes exactly one action.** The grant is keyed to a
//!    single `(session_id, request_id)` pair and is CONSUMED by the attempt,
//!    successful or not. A failed attempt must not leave a live grant lying
//!    around for the model's next call.
//! 2. **A stale click cannot authorize a newer request.** Both ids are checked.
//!    The model can issue a second tool call while the first card is still up;
//!    without the request-id check, the human's "yes" to a harmless action
//!    would silently authorize whatever replaced it.
//! 3. **Refusal is sticky.** A model that keeps asking must not be able to wear
//!    the user down by repetition, so a rejection holds for the whole session
//!    and only a deliberate, human-initiated reset ([`clear_refusal`]) lifts it.
//!
//! ## The master switch
//!
//! The whole execution path is dark unless `PETAL_AI_CONTROL=1` is set in the
//! environment. This is not a feature flag for taste: at the time of writing
//! nothing here has been exercised against a live model (the Gemini project is
//! out of credits), and code that clicks and types in a user's real
//! applications must not be reachable by default on the strength of unit tests
//! alone. With the switch off the tools are not even declared to the model, so
//! there is nothing to approve and nothing to execute.

use std::sync::{Mutex, MutexGuard, OnceLock};

use super::ax_digest::DigestIndex;
use super::control_policy::{Action, GrantScope, ScrollDirection, SafeKey, Standing};

/// Environment variable that arms window control. Anything other than exactly
/// `1` leaves it off — a typo must not enable it.
pub const CONTROL_ENV_VAR: &str = "PETAL_AI_CONTROL";

/// Pure form of the master switch, so the "unset means off" property is
/// testable without mutating process environment from a test.
pub fn control_enabled_for(raw: Option<&str>) -> bool {
    matches!(raw.map(str::trim), Some("1"))
}

/// Is agent control of the shared window armed at all?
///
/// Read live rather than cached: a session that starts after the operator sets
/// the variable should see it, and the cost is one `getenv` per tool call.
pub fn control_enabled() -> bool {
    control_enabled_for(std::env::var(CONTROL_ENV_VAR).ok().as_deref())
}

/// What the human is shown on the approval card. Deliberately explicit: for a
/// `window_type` this is the LITERAL text that will be typed, and for a
/// `window_click` it is the resolved element's role and title. A card that
/// said only "the AI wants to click something" would be consent theatre.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionDetail {
    /// One-line summary for the card's heading.
    pub summary: String,
    /// The exact text to be typed, when this is a type action. Rendered
    /// verbatim and never elided by the UI.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub literal_text: Option<String>,
    /// The resolved element, when this is a click.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub element: Option<String>,
}

/// Describe an action for the approval card. `resolved` is the element a click
/// cited, already looked up against the generation the model actually saw.
pub fn describe_action(action: &Action, resolved: Option<&DigestIndex>) -> ActionDetail {
    match action {
        Action::Type(text) => ActionDetail {
            summary: "Type into the shared window".to_string(),
            literal_text: Some(text.clone()),
            element: None,
        },
        Action::Click { .. } => {
            let element = resolved.map(describe_element);
            ActionDetail {
                summary: "Click in the shared window".to_string(),
                literal_text: None,
                element,
            }
        }
        Action::PressKey(key) => ActionDetail {
            summary: format!("Press {} in the shared window", key_label(*key)),
            literal_text: None,
            element: None,
        },
        Action::Scroll { direction, amount } => ActionDetail {
            summary: format!(
                "Scroll the shared window {} by {amount} line{}",
                direction_label(*direction),
                if *amount == 1 { "" } else { "s" }
            ),
            literal_text: None,
            element: None,
        },
    }
}

/// The line written to the room transcript for one attempted action.
///
/// Every attempt gets one, refusals included: a control feature whose audit
/// trail only records successes tells a room nothing about what was tried. The
/// literal typed text is included because that is what the room needs to see —
/// it is the same string the approving human was shown.
pub fn audit_line(tool: &str, detail: &ActionDetail, ok: bool, code: &str) -> String {
    let verb = if ok { "performed" } else { "refused" };
    let mut line = format!("Control: {verb} {tool}");
    if let Some(text) = detail.literal_text.as_deref() {
        line.push_str(&format!(" \u{2014} \u{201c}{text}\u{201d}"));
    } else if let Some(element) = detail.element.as_deref() {
        line.push_str(&format!(" \u{2014} {element}"));
    }
    if !ok {
        line.push_str(&format!(" ({code})"));
    }
    line
}

/// Role + title, which is as much as the digest retains about an element.
pub fn describe_element(index: &DigestIndex) -> String {
    match index.title.as_deref().filter(|title| !title.trim().is_empty()) {
        Some(title) => format!("{} \u{201c}{title}\u{201d}", index.role),
        None => index.role.clone(),
    }
}

fn key_label(key: SafeKey) -> &'static str {
    match key {
        SafeKey::Return => "Return",
        SafeKey::Tab => "Tab",
        SafeKey::Escape => "Escape",
        SafeKey::ArrowUp => "Arrow Up",
        SafeKey::ArrowDown => "Arrow Down",
        SafeKey::ArrowLeft => "Arrow Left",
        SafeKey::ArrowRight => "Arrow Right",
    }
}

fn direction_label(direction: ScrollDirection) -> &'static str {
    match direction {
        ScrollDirection::Up => "up",
        ScrollDirection::Down => "down",
        ScrollDirection::Left => "left",
        ScrollDirection::Right => "right",
    }
}

/// A request awaiting a human answer.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingRequest {
    pub session_id: u64,
    /// The model's function-call id. Half of the anti-staleness key.
    pub request_id: String,
    pub window_id: u32,
    pub tool: String,
    pub action: Action,
    pub detail: ActionDetail,
}

/// Everything the gate carries between a request and its execution.
#[derive(Debug, Default)]
pub struct ControlState {
    /// Bumped on every session start, so an answer aimed at a finished session
    /// can never land on its successor.
    pub session_id: u64,
    pub standing: Standing,
    pub pending: Option<PendingRequest>,
    /// The single outstanding "allow once" grant, keyed to the exact request it
    /// was given for.
    pub granted_once: Option<(u64, String)>,
}

/// How far the current answer authorizes this specific action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Authorization {
    /// A one-shot grant for exactly this request.
    Once,
    /// The human escalated to the whole session.
    Session,
    /// Nothing has been granted (or the grant belongs to another request).
    NotGranted,
    /// The human said no; sticky until deliberately reset.
    Refused,
}

/// Outcome of recording a human's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerOutcome {
    Applied,
    /// The click referred to a request that is no longer the pending one, or to
    /// a different session. Nothing was authorized.
    Stale,
}

/// Record an approval. Requires BOTH ids to match the pending request: the
/// model can replace the pending call between the card appearing and the human
/// clicking, and a yes must never carry over to whatever replaced it.
pub fn apply_approval(
    state: &mut ControlState,
    session_id: u64,
    request_id: &str,
    scope: GrantScope,
) -> AnswerOutcome {
    if session_id != state.session_id {
        return AnswerOutcome::Stale;
    }
    // A refused session stays refused; re-granting is a separate, deliberate
    // act (see `clear_refusal`) rather than something a stray click can undo.
    if state.standing == Standing::Refused {
        return AnswerOutcome::Stale;
    }
    let matches = state
        .pending
        .as_ref()
        .is_some_and(|pending| pending.session_id == session_id && pending.request_id == request_id);
    if !matches {
        return AnswerOutcome::Stale;
    }
    state.pending = None;
    match scope {
        GrantScope::Once => {
            state.granted_once = Some((session_id, request_id.to_string()));
        }
        GrantScope::Session => {
            state.standing = Standing::Session;
            state.granted_once = None;
        }
    }
    AnswerOutcome::Applied
}

/// Record a rejection. Sticky for the session.
///
/// Unlike an approval this does not require the request ids to match — only the
/// session. Refusing more than the human pointed at errs toward less agent
/// control, which is the safe direction; ignoring a "no" because the model had
/// meanwhile issued a different call is not.
pub fn apply_rejection(state: &mut ControlState, session_id: u64) -> AnswerOutcome {
    if session_id != state.session_id {
        return AnswerOutcome::Stale;
    }
    state.standing = Standing::Refused;
    state.granted_once = None;
    state.pending = None;
    AnswerOutcome::Applied
}

/// The deliberate re-grant path out of a sticky refusal. Only ever reached from
/// an explicit human action on the sharer's machine.
pub fn clear_refusal(state: &mut ControlState, session_id: u64) -> AnswerOutcome {
    if session_id != state.session_id {
        return AnswerOutcome::Stale;
    }
    if state.standing == Standing::Refused {
        state.standing = Standing::None;
    }
    AnswerOutcome::Applied
}

/// What this request is authorized to do right now.
pub fn authorization(state: &ControlState, session_id: u64, request_id: &str) -> Authorization {
    if session_id != state.session_id {
        return Authorization::NotGranted;
    }
    match state.standing {
        Standing::Refused => Authorization::Refused,
        Standing::Session => Authorization::Session,
        Standing::None => match state.granted_once.as_ref() {
            Some((granted_session, granted_request))
                if *granted_session == session_id && granted_request == request_id =>
            {
                Authorization::Once
            }
            _ => Authorization::NotGranted,
        },
    }
}

/// Spend a one-shot grant. Called as soon as an execution ATTEMPT begins, not
/// when it succeeds: an action that fails its moment-of-execution re-checks has
/// still consumed the human's yes, and leaving the grant live would let the
/// model retry into a window where the check happens to pass.
pub fn consume_once(state: &mut ControlState, session_id: u64, request_id: &str) {
    if state
        .granted_once
        .as_ref()
        .is_some_and(|(s, r)| *s == session_id && r == request_id)
    {
        state.granted_once = None;
    }
}

// ---- process-wide instance --------------------------------------------------

fn control_state() -> &'static Mutex<ControlState> {
    static STATE: OnceLock<Mutex<ControlState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(ControlState::default()))
}

fn lock() -> MutexGuard<'static, ControlState> {
    control_state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Run `f` against the process-wide gate state.
pub fn with_state<T>(f: impl FnOnce(&mut ControlState) -> T) -> T {
    f(&mut lock())
}

/// Start a new authorization epoch. Everything the previous session granted is
/// dropped, including a session-wide grant: consent does not survive the
/// session it was given in.
pub fn begin_session() -> u64 {
    let mut state = lock();
    state.session_id += 1;
    state.standing = Standing::None;
    state.pending = None;
    state.granted_once = None;
    state.session_id
}

/// Drop every grant. Called on teardown so nothing outlives the session.
pub fn end_session() {
    let mut state = lock();
    state.standing = Standing::None;
    state.pending = None;
    state.granted_once = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn armed(request: &str) -> ControlState {
        ControlState {
            session_id: 5,
            standing: Standing::None,
            pending: Some(PendingRequest {
                session_id: 5,
                request_id: request.to_string(),
                window_id: 1,
                tool: "window_type".to_string(),
                action: Action::Type("hi".into()),
                detail: describe_action(&Action::Type("hi".into()), None),
            }),
            granted_once: None,
        }
    }

    #[test]
    fn the_master_switch_is_off_unless_it_is_exactly_one() {
        assert!(control_enabled_for(Some("1")));
        assert!(control_enabled_for(Some(" 1 ")));
        for off in [None, Some(""), Some("0"), Some("true"), Some("yes"), Some("11")] {
            assert!(!control_enabled_for(off), "{off:?} must not arm control");
        }
    }

    #[test]
    fn with_the_switch_unset_the_process_reports_disabled() {
        // The suite never sets PETAL_AI_CONTROL, so this exercises the real
        // reader, not just the pure helper.
        if std::env::var(CONTROL_ENV_VAR).is_ok() {
            // A developer running with control armed would otherwise see a
            // confusing failure; the pure test above still covers the mapping.
            return;
        }
        assert!(!control_enabled());
    }

    #[test]
    fn allow_once_authorizes_exactly_one_action() {
        let mut state = armed("fc_1");
        assert_eq!(
            apply_approval(&mut state, 5, "fc_1", GrantScope::Once),
            AnswerOutcome::Applied
        );
        assert_eq!(authorization(&state, 5, "fc_1"), Authorization::Once);
        // A DIFFERENT call is not covered by the same yes.
        assert_eq!(authorization(&state, 5, "fc_2"), Authorization::NotGranted);
        // ... and the grant is spent by the attempt.
        consume_once(&mut state, 5, "fc_1");
        assert_eq!(authorization(&state, 5, "fc_1"), Authorization::NotGranted);
    }

    #[test]
    fn allow_for_session_authorizes_the_next_action_too() {
        let mut state = armed("fc_1");
        assert_eq!(
            apply_approval(&mut state, 5, "fc_1", GrantScope::Session),
            AnswerOutcome::Applied
        );
        assert_eq!(authorization(&state, 5, "fc_1"), Authorization::Session);
        assert_eq!(authorization(&state, 5, "fc_99"), Authorization::Session);
        // Consuming a one-shot grant must not disturb a session grant.
        consume_once(&mut state, 5, "fc_1");
        assert_eq!(authorization(&state, 5, "fc_99"), Authorization::Session);
    }

    #[test]
    fn a_stale_request_id_cannot_authorize_a_newer_request() {
        // The model replaced the pending call while the card was up.
        let mut state = armed("fc_new");
        assert_eq!(
            apply_approval(&mut state, 5, "fc_old", GrantScope::Once),
            AnswerOutcome::Stale
        );
        assert_eq!(authorization(&state, 5, "fc_new"), Authorization::NotGranted);
        assert_eq!(authorization(&state, 5, "fc_old"), Authorization::NotGranted);
        // The pending request survives an answer that did not match it.
        assert!(state.pending.is_some());
    }

    #[test]
    fn a_stale_session_id_cannot_authorize_anything() {
        let mut state = armed("fc_1");
        assert_eq!(
            apply_approval(&mut state, 4, "fc_1", GrantScope::Session),
            AnswerOutcome::Stale
        );
        assert_eq!(authorization(&state, 5, "fc_1"), Authorization::NotGranted);
        // And an answer for a dead session cannot refuse the live one either.
        assert_eq!(apply_rejection(&mut state, 4), AnswerOutcome::Stale);
        assert_eq!(state.standing, Standing::None);
    }

    #[test]
    fn a_grant_never_survives_its_session() {
        let mut state = armed("fc_1");
        apply_approval(&mut state, 5, "fc_1", GrantScope::Session);
        // A later session (higher epoch) inherits nothing.
        assert_eq!(authorization(&state, 6, "fc_1"), Authorization::NotGranted);
    }

    #[test]
    fn rejection_is_sticky_and_only_a_deliberate_reset_lifts_it() {
        let mut state = armed("fc_1");
        assert_eq!(apply_rejection(&mut state, 5), AnswerOutcome::Applied);
        assert_eq!(state.standing, Standing::Refused);
        assert!(state.pending.is_none());
        assert_eq!(authorization(&state, 5, "fc_1"), Authorization::Refused);

        // A later approval — for any request — cannot undo it.
        state.pending = armed("fc_2").pending;
        assert_eq!(
            apply_approval(&mut state, 5, "fc_2", GrantScope::Session),
            AnswerOutcome::Stale
        );
        assert_eq!(authorization(&state, 5, "fc_2"), Authorization::Refused);

        // Only the explicit reset does.
        assert_eq!(clear_refusal(&mut state, 5), AnswerOutcome::Applied);
        assert_eq!(state.standing, Standing::None);
        assert_eq!(authorization(&state, 5, "fc_2"), Authorization::NotGranted);
    }

    #[test]
    fn rejection_beats_a_live_one_shot_grant() {
        // Approve, then reject before the action ran: the grant must be gone.
        let mut state = armed("fc_1");
        apply_approval(&mut state, 5, "fc_1", GrantScope::Once);
        apply_rejection(&mut state, 5);
        assert_eq!(authorization(&state, 5, "fc_1"), Authorization::Refused);
        assert!(state.granted_once.is_none());
    }

    #[test]
    fn the_card_shows_the_literal_text_and_the_resolved_element() {
        let typed = describe_action(&Action::Type("wire $500".into()), None);
        assert_eq!(typed.literal_text.as_deref(), Some("wire $500"));

        let element = DigestIndex {
            role: "AXButton".into(),
            title: Some("Send".into()),
            ancestor_path: Vec::new(),
            rect: None,
        };
        let click = describe_action(
            &Action::Click {
                generation: 1,
                element_index: 3,
            },
            Some(&element),
        );
        assert_eq!(click.element.as_deref(), Some("AXButton \u{201c}Send\u{201d}"));

        // An unresolvable element leaves the field empty rather than inventing
        // a description — the caller refuses such a call outright.
        let unresolved = describe_action(
            &Action::Click {
                generation: 1,
                element_index: 3,
            },
            None,
        );
        assert!(unresolved.element.is_none());

        let key = describe_action(&Action::PressKey(SafeKey::Return), None);
        assert!(key.summary.contains("Return"), "{}", key.summary);
        let scroll = describe_action(
            &Action::Scroll {
                direction: ScrollDirection::Down,
                amount: 3,
            },
            None,
        );
        assert!(scroll.summary.contains("down"), "{}", scroll.summary);
        assert!(scroll.summary.contains('3'), "{}", scroll.summary);
    }

    #[test]
    fn the_room_sees_refusals_as_well_as_successes() {
        let typed = describe_action(&Action::Type("wire $500".into()), None);
        let ran = audit_line("window_type", &typed, true, "ok");
        assert!(ran.contains("performed window_type"), "{ran}");
        assert!(ran.contains("wire $500"), "{ran}");
        // A refusal names its reason so the trail explains itself.
        let refused = audit_line("window_type", &typed, false, "secure_input_active");
        assert!(refused.contains("refused window_type"), "{refused}");
        assert!(refused.contains("secure_input_active"), "{refused}");
    }

    #[test]
    fn beginning_a_session_drops_every_previous_grant() {
        let first = begin_session();
        with_state(|state| {
            state.standing = Standing::Session;
            state.granted_once = Some((first, "fc_1".into()));
        });
        let second = begin_session();
        assert!(second > first);
        with_state(|state| {
            assert_eq!(state.standing, Standing::None);
            assert!(state.granted_once.is_none());
            assert!(state.pending.is_none());
            assert_eq!(authorization(state, first, "fc_1"), Authorization::NotGranted);
        });
        end_session();
    }
}
