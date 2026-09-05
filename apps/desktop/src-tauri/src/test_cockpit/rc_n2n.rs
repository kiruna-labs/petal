//! RC-N2N / RC-N2W (journey RC-07, #819): Petal as the CONTROLLER.
//!
//! `scripts/rc-live-suite.sh` proves web->native control exhaustively. Nothing
//! drove the reverse — a NATIVE controller — or native<->native control, which
//! is the direction two Petal users at two Macs actually use. This module holds
//! the parts of that scenario that can be decided without a live rig: the
//! keystone gesture plan, the script that drives the REAL control route, and
//! the pass/fail oracle over what the run observed.
//!
//! ## Why the drive is a DOM dispatch and not a Rust-built draft
//!
//! The controller path under test is `apps/desktop/src/routes/compositor/
//! control/+page.svelte`: it turns DOM pointer/keyboard events into a
//! `RemoteControlDraft` and calls `remote_control_send`. Building that draft in
//! Rust would skip the only part of the controller that is not already covered
//! by the web suite. So the scenario dispatches real `PointerEvent`s at the
//! `.control-overlay` element and real `KeyboardEvent`s at `window` (that is
//! where the route listens), and reads back what the route actually published
//! from `remote_control::cockpit_ledger`.
//!
//! Two properties of the route make synthetic dispatch work, and both are
//! load-bearing — if either changes, this scenario stops driving anything:
//! `setPointerCapture` is wrapped in try/catch ("best effort for synthetic/
//! non-primary events"), and the key listeners are on `window`, not on a
//! focused element.

/// Typed by the controller and expected to land in the sacrificial document.
/// Letters only on purpose: a digit would need a different `KeyboardEvent.code`
/// mapping and buys nothing.
pub(crate) const KEYSTONE_TEXT: &str = "petalrcnative";

/// Gap between characters of the typed keystone string, in milliseconds.
/// `KEYSTONE_TEXT.len() * KEYSTROKE_INTERVAL_MS` must stay well inside the
/// settle window the scenario waits after each drive step.
pub(crate) const KEYSTROKE_INTERVAL_MS: u32 = 20;

/// How long the run lets each drive step settle before the next one. It has to
/// outlast the paced typing above, or the last characters land after the run
/// has moved on and the document reads short.
pub(crate) const DRIVE_STEP_SETTLE_MS: u32 = 900;

/// One step of the keystone set: click, type, Cmd+A, normalized Copy,
/// normalized Paste, release. Each is dispatched separately so the run can
/// settle between them and so the publish ledger has unambiguous boundaries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DriveStep {
    /// A left click at the centre of the remote window.
    Click,
    /// Type `text` one key at a time, exactly as a user's keyboard would.
    Type(String),
    /// A Command chord (Cmd+A, Cmd+C, Cmd+V); Copy/Paste are normalized by
    /// the native control overlay, while Cmd+A remains ordinary key input.
    MetaChord(char),
    /// Escape — the route's own release path, which invokes
    /// `remote_control_set_active(active: false)`. Releasing through the real
    /// key handler rather than a second Rust call keeps the release in the
    /// same code path a user takes.
    ReleaseViaEscape,
}

/// The ordered keystone plan. Kept as one function so the scenario, the docs
/// and the tests cannot drift apart.
pub(crate) fn keystone_steps() -> Vec<DriveStep> {
    vec![
        DriveStep::Click,
        DriveStep::Type(KEYSTONE_TEXT.to_string()),
        DriveStep::MetaChord('a'),
        DriveStep::MetaChord('c'),
        DriveStep::MetaChord('v'),
        DriveStep::ReleaseViaEscape,
    ]
}

/// Browser peers intentionally do not implement the native clipboard protocol,
/// so the delivery-only scenario exercises only ordinary remote-control input.
pub(crate) fn delivery_keystone_steps() -> Vec<DriveStep> {
    vec![
        DriveStep::Click,
        DriveStep::Type(KEYSTONE_TEXT.to_string()),
        DriveStep::MetaChord('a'),
        DriveStep::ReleaseViaEscape,
    ]
}

/// Short label used in evidence records and failure messages.
pub(crate) fn step_id(step: &DriveStep) -> String {
    match step {
        DriveStep::Click => "click".to_string(),
        DriveStep::Type(_) => "type".to_string(),
        DriveStep::MetaChord(key) => format!("cmd-{key}"),
        DriveStep::ReleaseViaEscape => "release".to_string(),
    }
}

fn js_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `KeyboardEvent.code` for a single character, matching what a real US layout
/// reports. Only the characters the keystone set uses need to be right.
fn key_code_for(ch: char) -> String {
    if ch.is_ascii_alphabetic() {
        format!("Key{}", ch.to_ascii_uppercase())
    } else if ch.is_ascii_digit() {
        format!("Digit{ch}")
    } else {
        String::new()
    }
}

/// The script evaluated inside the control overlay webview for one step.
///
/// It must never invoke a Tauri command itself: the whole point is that the
/// Svelte route decides what to publish. `drive_script_never_invokes_the_send_command`
/// is the guard that keeps a future edit from quietly turning this back into
/// wire injection.
pub(crate) fn drive_script(step: &DriveStep) -> String {
    match step {
        DriveStep::Click => "(() => {\n\
             const el = document.querySelector('.control-overlay');\n\
             if (!el) return;\n\
             const r = el.getBoundingClientRect();\n\
             if (!(r.width > 0 && r.height > 0)) return;\n\
             const x = r.left + r.width / 2;\n\
             const y = r.top + r.height / 2;\n\
             const p = (type, extra) => el.dispatchEvent(new PointerEvent(type, Object.assign({\n\
               bubbles: true, cancelable: true, composed: true, pointerId: 1,\n\
               pointerType: 'mouse', isPrimary: true, clientX: x, clientY: y, detail: 1\n\
             }, extra)));\n\
             p('pointerdown', { button: 0, buttons: 1 });\n\
             p('pointerup', { button: 0, buttons: 0 });\n\
           })();"
            .to_string(),
        // Paced, not blasted. A tight loop hands the host a burst its bounded
        // coalescing replay queue is entitled to collapse, and a dropped
        // character would read as "the text never landed" -- a product verdict
        // the run has no business drawing from its own typing speed. The pace
        // is well inside the settle window the scenario waits after each step.
        DriveStep::Type(text) => format!(
            "(async () => {{\n\
               const k = (type, init) => window.dispatchEvent(new KeyboardEvent(type, Object.assign({{\n\
                 bubbles: true, cancelable: true\n\
               }}, init)));\n\
               const pause = () => new Promise((resolve) => setTimeout(resolve, {}));\n\
               for (const entry of {}) {{\n\
                 k('keydown', {{ key: entry[0], code: entry[1] }});\n\
                 k('keyup', {{ key: entry[0], code: entry[1] }});\n\
                 await pause();\n\
               }}\n\
             }})();",
            KEYSTROKE_INTERVAL_MS,
            js_key_entries(text)
        ),
        DriveStep::MetaChord(key) => format!(
            "(() => {{\n\
               const init = {{ key: {}, code: {}, metaKey: true, bubbles: true, cancelable: true }};\n\
               window.dispatchEvent(new KeyboardEvent('keydown', init));\n\
               window.dispatchEvent(new KeyboardEvent('keyup', init));\n\
             }})();",
            js_string(&key.to_string()),
            js_string(&key_code_for(*key))
        ),
        DriveStep::ReleaseViaEscape => "(() => {\n\
             const init = { key: 'Escape', code: 'Escape', bubbles: true, cancelable: true };\n\
             window.dispatchEvent(new KeyboardEvent('keydown', init));\n\
             window.dispatchEvent(new KeyboardEvent('keyup', init));\n\
           })();"
            .to_string(),
    }
}

fn js_key_entries(text: &str) -> String {
    let entries: Vec<String> = text
        .chars()
        .map(|ch| {
            format!(
                "[{}, {}]",
                js_string(&ch.to_string()),
                js_string(&key_code_for(ch))
            )
        })
        .collect();
    format!("[{}]", entries.join(", "))
}

/// One message the CONTROLLER actually published, projected from
/// `remote_control::cockpit_ledger`. Kept as a plain struct so the oracle stays
/// testable without a live control stack.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DrivenInput {
    /// Wire kind: "request" | "release" | "pointer" | "key" | "text" |
    /// "wheel" | metadata-only native "copy" | "paste".
    pub kind: String,
    /// "down" | "up" | "move" | "click" for pointer/key messages.
    pub action: Option<String>,
    pub key: Option<String>,
    pub meta: bool,
    pub t_ms: u64,
}

/// A `remote-control-status` the controller received.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GrantStatus {
    pub status: String,
    pub has_grant_token: bool,
    pub t_ms: u64,
}

/// What the HOST did with an input: its own terminal disposition, not a wire
/// echo. Reported by the test-peer over the authenticated cockpit socket.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HostEffect {
    pub kind: String,
    pub action: Option<String>,
    pub key: Option<String>,
    /// "applied" | "replayFailed" | "injectionTimeout" | "superseded".
    pub outcome: String,
    pub t_ms: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum RcVerdict {
    Pass(String),
    /// A real product failure.
    TestFail(String),
    /// The instrument could not measure. Never a product failure — see the
    /// #821 rule ("an oracle that cannot listen must never report silence").
    InfraFail(String),
}

pub(crate) struct RcN2nObservations<'a> {
    pub driven: &'a [DrivenInput],
    pub statuses: &'a [GrantStatus],
    pub host_effects: &'a [HostEffect],
    /// Pressed buttons/keys the host still holds after the release.
    pub pressed_inputs_after: usize,
    /// Control sessions the host still holds after the release.
    pub sessions_after: usize,
    /// Text of the sacrificial document, read on the host side.
    pub document_text: &'a str,
    /// Selected text after the Cmd+A, if the host could read it.
    pub selection_after_select_all: Option<&'a str>,
    pub expected_text: &'a str,
}

fn is_input(kind: &str) -> bool {
    matches!(kind, "pointer" | "key" | "text" | "wheel" | "copy" | "paste")
}

fn is_delivery_input(kind: &str) -> bool {
    matches!(kind, "pointer" | "key" | "text" | "wheel")
}

/// The pass bar for RC-N2N. Ordered so the most diagnostic failure wins: a run
/// that never drove anything must not be reported as "the text did not land".
pub(crate) fn evaluate(obs: RcN2nObservations<'_>) -> RcVerdict {
    if obs.driven.is_empty() {
        return RcVerdict::InfraFail(
            "the controller publish ledger is empty -- not even the control request was recorded, \
             so the ledger itself never armed"
                .to_string(),
        );
    }
    let requested = obs.driven.iter().any(|input| input.kind == "request");
    let inputs: Vec<&DrivenInput> = obs
        .driven
        .iter()
        .filter(|input| is_input(&input.kind))
        .collect();
    if !requested {
        return RcVerdict::InfraFail(
            "no control request reached the ledger; the scenario never asked for control"
                .to_string(),
        );
    }
    let granted = obs
        .statuses
        .iter()
        .any(|status| status.status == "active" && status.has_grant_token);
    if !granted {
        let seen: Vec<&str> = obs
            .statuses
            .iter()
            .map(|status| status.status.as_str())
            .collect();
        return RcVerdict::TestFail(format!(
            "the host never granted control: no 'active' status carrying a grant token (saw: {})",
            if seen.is_empty() {
                "nothing".to_string()
            } else {
                seen.join(", ")
            }
        ));
    }
    if inputs.is_empty() {
        // The request was published from Rust and the grant landed, so the
        // ledger and the transport both work -- what produced nothing is the
        // DOM drive. `eval` is fire-and-forget, so this genuinely cannot tell a
        // failed eval from a control route that stopped publishing. Report the
        // ambiguity rather than picking the flattering half.
        return RcVerdict::InfraFail(
            "the control overlay published no input drafts. The grant landed, so transport and \
             ledger are fine; either the injected drive script did not run (eval is \
             fire-and-forget and reports nothing) or the compositor/control route no longer \
             publishes. Re-run with the overlay's web inspector open to tell them apart"
                .to_string(),
        );
    }
    let pointer_down = inputs
        .iter()
        .any(|input| input.kind == "pointer" && input.action.as_deref() == Some("down"));
    let pointer_up = inputs
        .iter()
        .any(|input| input.kind == "pointer" && input.action.as_deref() == Some("up"));
    if !(pointer_down && pointer_up) {
        return RcVerdict::InfraFail(
            "the click gesture did not reach the control route (no pointer down+up pair in the \
             publish ledger)"
                .to_string(),
        );
    }
    for chord in ['a', 'c'] {
        let sent = inputs.iter().any(|input| {
            (input.kind == "key" && input.meta && input.key.as_deref() == Some(&chord.to_string()))
                || (chord == 'c' && input.kind == "copy")
        });
        if !sent {
            return RcVerdict::InfraFail(format!(
                "the Cmd+{chord} operation did not reach the control route"
            ));
        }
    }
    if inputs.iter().any(|input| input.kind == "copy")
        && inputs.iter().any(|input| {
            input.kind == "key"
                && input.meta
                && matches!(input.key.as_deref(), Some("c") | Some("v"))
        })
    {
        return RcVerdict::TestFail(
            "native clipboard operation was also published as a raw Meta+C/V key".to_string(),
        );
    }
    if !obs.driven.iter().any(|input| input.kind == "release") {
        return RcVerdict::TestFail(
            "Escape did not release control: the route published no release message".to_string(),
        );
    }

    if obs.host_effects.is_empty() {
        return RcVerdict::TestFail(format!(
            "the controller published {} inputs under a live grant and the host replayed none",
            inputs.len()
        ));
    }
    if let Some(bad) = obs
        .host_effects
        .iter()
        .find(|effect| effect.outcome != "applied")
    {
        return RcVerdict::TestFail(format!(
            "the host refused an input it had granted: kind={} action={:?} outcome={}",
            bad.kind, bad.action, bad.outcome
        ));
    }
    for input in &inputs {
        let matched = obs.host_effects.iter().any(|effect| {
            effect.kind == input.kind && effect.action.as_deref() == input.action.as_deref()
        });
        if !matched {
            return RcVerdict::TestFail(format!(
                "the host never replayed a published {} {:?} input",
                input.kind, input.action
            ));
        }
    }

    if !obs.document_text.contains(obs.expected_text) {
        return RcVerdict::TestFail(format!(
            "the typed text never landed in the sacrificial document: expected it to contain \
             '{}', read '{}'",
            obs.expected_text, obs.document_text
        ));
    }
    match obs.selection_after_select_all {
        Some(selection) if selection.contains(obs.expected_text) => {}
        Some(selection) => {
            return RcVerdict::TestFail(format!(
                "Cmd+A did not select the document: selection was '{selection}'"
            ))
        }
        None => {
            return RcVerdict::InfraFail(
                "the host could not read the document selection, so Cmd+A's effect is unmeasured"
                    .to_string(),
            )
        }
    }

    if obs.pressed_inputs_after > 0 {
        return RcVerdict::TestFail(format!(
            "{} pressed input(s) were still held on the host after the release",
            obs.pressed_inputs_after
        ));
    }
    if obs.sessions_after > 0 {
        return RcVerdict::TestFail(format!(
            "{} control session(s) survived the release on the host",
            obs.sessions_after
        ));
    }

    RcVerdict::Pass(format!(
        "native controller drove {} inputs through the real compositor/control route; the host \
         granted control, replayed every one as applied, '{}' landed in the sacrificial document, \
         Cmd+A selected it, and the release left no held input or session behind",
        inputs.len(),
        obs.expected_text
    ))
}

/// What the native->web leg (RC-N2W) can and cannot prove. A browser cannot
/// inject OS input, so its ledger is a DELIVERY record: the controller's
/// request, grant handshake and inputs arrived intact at a remote peer. It is
/// never evidence that anything was applied, and the scenario says so out loud
/// rather than borrowing RC-N2N's language.
pub(crate) fn evaluate_delivery_only(
    driven: &[DrivenInput],
    received_kinds: &[String],
    granted: bool,
) -> RcVerdict {
    if driven.is_empty() {
        return RcVerdict::InfraFail(
            "the controller publish ledger is empty; the ledger never armed".to_string(),
        );
    }
    if !granted {
        return RcVerdict::TestFail(
            "the web peer never granted control in response to the native request".to_string(),
        );
    }
    let published: Vec<&DrivenInput> = driven
        .iter()
        .filter(|input| is_delivery_input(&input.kind))
        .collect();
    if published.is_empty() {
        return RcVerdict::InfraFail(
            "the control overlay published no input drafts; eval is fire-and-forget, so this \
             cannot distinguish a failed drive from a broken route"
                .to_string(),
        );
    }
    for input in &published {
        if !received_kinds.iter().any(|kind| kind == &input.kind) {
            return RcVerdict::TestFail(format!(
                "the web peer never received a published {} input",
                input.kind
            ));
        }
    }
    RcVerdict::Pass(format!(
        "the native controller's grant handshake and {} inputs arrived intact at the web peer \
         (DELIVERY only -- a browser cannot inject OS input, so nothing here proves an input was \
         applied)",
        published.len()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(kind: &str, action: Option<&str>, key: Option<&str>, meta: bool) -> DrivenInput {
        DrivenInput {
            kind: kind.to_string(),
            action: action.map(str::to_string),
            key: key.map(str::to_string),
            meta,
            t_ms: 0,
        }
    }

    fn effect(kind: &str, action: Option<&str>, outcome: &str) -> HostEffect {
        HostEffect {
            kind: kind.to_string(),
            action: action.map(str::to_string),
            key: None,
            outcome: outcome.to_string(),
            t_ms: 0,
        }
    }

    fn granted_status() -> GrantStatus {
        GrantStatus {
            status: "active".to_string(),
            has_grant_token: true,
            t_ms: 0,
        }
    }

    fn healthy_driven() -> Vec<DrivenInput> {
        let mut driven = vec![input("request", None, None, false)];
        driven.push(input("pointer", Some("down"), None, false));
        driven.push(input("pointer", Some("up"), None, false));
        for ch in KEYSTONE_TEXT.chars() {
            driven.push(input("key", Some("down"), Some(&ch.to_string()), false));
            driven.push(input("key", Some("up"), Some(&ch.to_string()), false));
        }
        driven.push(input("key", Some("down"), Some("a"), true));
        driven.push(input("key", Some("up"), Some("a"), true));
        driven.push(input("key", Some("down"), Some("c"), true));
        driven.push(input("key", Some("up"), Some("c"), true));
        driven.push(input("release", None, None, false));
        driven
    }

    fn healthy_effects() -> Vec<HostEffect> {
        vec![
            effect("pointer", Some("down"), "applied"),
            effect("pointer", Some("up"), "applied"),
            effect("key", Some("down"), "applied"),
            effect("key", Some("up"), "applied"),
        ]
    }

    fn normalized_clipboard_driven() -> Vec<DrivenInput> {
        let mut driven = healthy_driven();
        driven.retain(|entry| {
            !(entry.kind == "key" && entry.meta && entry.key.as_deref() == Some("c"))
        });
        driven.push(input("copy", None, None, false));
        driven.push(input("paste", None, None, false));
        driven
    }

    fn observations<'a>(
        driven: &'a [DrivenInput],
        statuses: &'a [GrantStatus],
        effects: &'a [HostEffect],
        document_text: &'a str,
        selection: Option<&'a str>,
    ) -> RcN2nObservations<'a> {
        RcN2nObservations {
            driven,
            statuses,
            host_effects: effects,
            pressed_inputs_after: 0,
            sessions_after: 0,
            document_text,
            selection_after_select_all: selection,
            expected_text: KEYSTONE_TEXT,
        }
    }

    #[test]
    fn a_healthy_run_passes() {
        let driven = healthy_driven();
        let statuses = vec![granted_status()];
        let effects = healthy_effects();
        let verdict = evaluate(observations(
            &driven,
            &statuses,
            &effects,
            KEYSTONE_TEXT,
            Some(KEYSTONE_TEXT),
        ));
        assert!(
            matches!(verdict, RcVerdict::Pass(_)),
            "expected pass, got {verdict:?}"
        );
    }

    #[test]
    fn normalized_clipboard_operations_are_metadata_only_but_still_checked_end_to_end() {
        let driven = normalized_clipboard_driven();
        let statuses = vec![granted_status()];
        let mut effects = healthy_effects();
        effects.push(effect("copy", None, "applied"));
        effects.push(effect("paste", None, "applied"));
        let verdict = evaluate(observations(
            &driven,
            &statuses,
            &effects,
            KEYSTONE_TEXT,
            Some(KEYSTONE_TEXT),
        ));
        assert!(matches!(verdict, RcVerdict::Pass(_)), "{verdict:?}");
    }

    #[test]
    fn normalized_clipboard_operation_rejects_a_duplicate_raw_chord() {
        let mut driven = normalized_clipboard_driven();
        driven.push(input("key", Some("down"), Some("c"), true));
        let statuses = vec![granted_status()];
        let mut effects = healthy_effects();
        effects.push(effect("copy", None, "applied"));
        effects.push(effect("paste", None, "applied"));
        let verdict = evaluate(observations(
            &driven,
            &statuses,
            &effects,
            KEYSTONE_TEXT,
            Some(KEYSTONE_TEXT),
        ));
        assert!(matches!(verdict, RcVerdict::TestFail(_)), "{verdict:?}");
    }

    #[test]
    fn an_empty_ledger_is_an_instrument_failure_not_a_product_failure() {
        let statuses = vec![granted_status()];
        let verdict = evaluate(observations(&[], &statuses, &[], "", None));
        assert!(matches!(verdict, RcVerdict::InfraFail(_)), "{verdict:?}");
    }

    #[test]
    fn a_refused_grant_is_a_product_failure() {
        let driven = healthy_driven();
        let statuses = vec![GrantStatus {
            status: "denied".to_string(),
            has_grant_token: false,
            t_ms: 0,
        }];
        let effects = healthy_effects();
        let verdict = evaluate(observations(
            &driven,
            &statuses,
            &effects,
            KEYSTONE_TEXT,
            Some(KEYSTONE_TEXT),
        ));
        match verdict {
            RcVerdict::TestFail(detail) => {
                assert!(detail.contains("never granted control"), "{detail}");
                assert!(detail.contains("denied"), "must name what it did see: {detail}");
            }
            other => panic!("expected TestFail, got {other:?}"),
        }
    }

    #[test]
    fn an_active_status_without_a_grant_token_is_not_a_grant() {
        let driven = healthy_driven();
        let statuses = vec![GrantStatus {
            status: "active".to_string(),
            has_grant_token: false,
            t_ms: 0,
        }];
        let effects = healthy_effects();
        let verdict = evaluate(observations(
            &driven,
            &statuses,
            &effects,
            KEYSTONE_TEXT,
            Some(KEYSTONE_TEXT),
        ));
        assert!(matches!(verdict, RcVerdict::TestFail(_)), "{verdict:?}");
    }

    #[test]
    fn a_granted_run_that_published_nothing_reports_the_ambiguity() {
        let driven = vec![input("request", None, None, false)];
        let statuses = vec![granted_status()];
        let verdict = evaluate(observations(&driven, &statuses, &[], "", None));
        match verdict {
            RcVerdict::InfraFail(detail) => {
                assert!(detail.contains("fire-and-forget"), "{detail}");
                assert!(
                    detail.contains("no longer"),
                    "must admit the route could be the cause: {detail}"
                );
            }
            other => panic!("expected InfraFail, got {other:?}"),
        }
    }

    #[test]
    fn published_but_never_replayed_is_a_product_failure() {
        let driven = healthy_driven();
        let statuses = vec![granted_status()];
        let verdict = evaluate(observations(
            &driven,
            &statuses,
            &[],
            KEYSTONE_TEXT,
            Some(KEYSTONE_TEXT),
        ));
        match verdict {
            RcVerdict::TestFail(detail) => assert!(detail.contains("replayed none"), "{detail}"),
            other => panic!("expected TestFail, got {other:?}"),
        }
    }

    #[test]
    fn a_refused_input_under_a_live_grant_is_a_product_failure() {
        let driven = healthy_driven();
        let statuses = vec![granted_status()];
        let mut effects = healthy_effects();
        effects.push(effect("key", Some("down"), "replayFailed"));
        let verdict = evaluate(observations(
            &driven,
            &statuses,
            &effects,
            KEYSTONE_TEXT,
            Some(KEYSTONE_TEXT),
        ));
        match verdict {
            RcVerdict::TestFail(detail) => assert!(detail.contains("replayFailed"), "{detail}"),
            other => panic!("expected TestFail, got {other:?}"),
        }
    }

    #[test]
    fn a_replayed_pointer_does_not_vouch_for_an_unreplayed_key() {
        let driven = healthy_driven();
        let statuses = vec![granted_status()];
        let effects = vec![
            effect("pointer", Some("down"), "applied"),
            effect("pointer", Some("up"), "applied"),
        ];
        let verdict = evaluate(observations(
            &driven,
            &statuses,
            &effects,
            KEYSTONE_TEXT,
            Some(KEYSTONE_TEXT),
        ));
        match verdict {
            RcVerdict::TestFail(detail) => assert!(detail.contains("never replayed"), "{detail}"),
            other => panic!("expected TestFail, got {other:?}"),
        }
    }

    #[test]
    fn text_that_never_landed_is_a_product_failure_even_with_a_clean_host_ledger() {
        let driven = healthy_driven();
        let statuses = vec![granted_status()];
        let effects = healthy_effects();
        let verdict = evaluate(observations(&driven, &statuses, &effects, "", Some("")));
        match verdict {
            RcVerdict::TestFail(detail) => assert!(detail.contains("never landed"), "{detail}"),
            other => panic!("expected TestFail, got {other:?}"),
        }
    }

    #[test]
    fn an_unreadable_selection_is_unmeasured_not_failed() {
        let driven = healthy_driven();
        let statuses = vec![granted_status()];
        let effects = healthy_effects();
        let verdict = evaluate(observations(
            &driven,
            &statuses,
            &effects,
            KEYSTONE_TEXT,
            None,
        ));
        assert!(matches!(verdict, RcVerdict::InfraFail(_)), "{verdict:?}");
    }

    #[test]
    fn a_stuck_pressed_input_after_release_fails_the_run() {
        let driven = healthy_driven();
        let statuses = vec![granted_status()];
        let effects = healthy_effects();
        let mut obs = observations(
            &driven,
            &statuses,
            &effects,
            KEYSTONE_TEXT,
            Some(KEYSTONE_TEXT),
        );
        obs.pressed_inputs_after = 1;
        match evaluate(obs) {
            RcVerdict::TestFail(detail) => assert!(detail.contains("still held"), "{detail}"),
            other => panic!("expected TestFail, got {other:?}"),
        }
    }

    #[test]
    fn a_session_that_survives_release_fails_the_run() {
        let driven = healthy_driven();
        let statuses = vec![granted_status()];
        let effects = healthy_effects();
        let mut obs = observations(
            &driven,
            &statuses,
            &effects,
            KEYSTONE_TEXT,
            Some(KEYSTONE_TEXT),
        );
        obs.sessions_after = 2;
        match evaluate(obs) {
            RcVerdict::TestFail(detail) => assert!(detail.contains("survived the release"), "{detail}"),
            other => panic!("expected TestFail, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_release_message_fails_the_run() {
        let driven: Vec<DrivenInput> = healthy_driven()
            .into_iter()
            .filter(|input| input.kind != "release")
            .collect();
        let statuses = vec![granted_status()];
        let effects = healthy_effects();
        match evaluate(observations(
            &driven,
            &statuses,
            &effects,
            KEYSTONE_TEXT,
            Some(KEYSTONE_TEXT),
        )) {
            RcVerdict::TestFail(detail) => assert!(detail.contains("release"), "{detail}"),
            other => panic!("expected TestFail, got {other:?}"),
        }
    }

    #[test]
    fn drive_script_never_invokes_the_send_command() {
        // The guard that keeps this scenario honest: the moment the drive
        // script calls the Tauri command itself, it stops exercising the
        // controller route and starts impersonating it.
        for step in keystone_steps() {
            let script = drive_script(&step);
            for forbidden in [
                "remote_control_send",
                "remoteControlSend",
                "__TAURI__",
                "__TAURI_INTERNALS__",
                "invoke(",
            ] {
                assert!(
                    !script.contains(forbidden),
                    "{:?} script must not contain {forbidden}: {script}",
                    step_id(&step)
                );
            }
        }
    }

    #[test]
    fn click_script_dispatches_a_real_pointer_pair_at_the_overlay() {
        let script = drive_script(&DriveStep::Click);
        assert!(script.contains(".control-overlay"), "{script}");
        assert!(script.contains("new PointerEvent"), "{script}");
        assert!(script.contains("'pointerdown'"), "{script}");
        assert!(script.contains("'pointerup'"), "{script}");
        assert!(
            script.contains("getBoundingClientRect"),
            "coordinates must come from the real element rect: {script}"
        );
    }

    #[test]
    fn type_script_emits_one_key_pair_per_character_with_real_codes() {
        let script = drive_script(&DriveStep::Type("ab".to_string()));
        assert!(script.contains("[\"a\", \"KeyA\"]"), "{script}");
        assert!(script.contains("[\"b\", \"KeyB\"]"), "{script}");
        assert!(script.contains("'keydown'"), "{script}");
        assert!(script.contains("'keyup'"), "{script}");
        assert!(
            script.contains("window.dispatchEvent"),
            "the route listens on window, not on the overlay: {script}"
        );
    }

    #[test]
    fn typing_is_paced_and_finishes_inside_the_settle_window() {
        let script = drive_script(&DriveStep::Type(KEYSTONE_TEXT.to_string()));
        assert!(
            script.contains(&format!("setTimeout(resolve, {KEYSTROKE_INTERVAL_MS})")),
            "a burst can be collapsed by the host's coalescing replay queue, and a dropped \
             character would read as a product failure: {script}"
        );
        let typing_ms = KEYSTONE_TEXT.chars().count() as u32 * KEYSTROKE_INTERVAL_MS;
        assert!(
            typing_ms < DRIVE_STEP_SETTLE_MS,
            "typing takes {typing_ms}ms but each step only settles for {DRIVE_STEP_SETTLE_MS}ms, \
             so the last characters would land after the run stopped waiting"
        );
    }

    #[test]
    fn meta_chord_script_sets_the_command_modifier() {
        let script = drive_script(&DriveStep::MetaChord('a'));
        assert!(script.contains("metaKey: true"), "{script}");
        assert!(script.contains("\"a\""), "{script}");
        assert!(script.contains("\"KeyA\""), "{script}");
    }

    #[test]
    fn release_script_uses_escape_so_the_route_owns_the_release() {
        let script = drive_script(&DriveStep::ReleaseViaEscape);
        assert!(script.contains("'Escape'"), "{script}");
        assert!(script.contains("window.dispatchEvent"), "{script}");
    }

    #[test]
    fn js_string_escapes_quotes_and_backslashes() {
        assert_eq!(js_string("a\"b\\c"), "\"a\\\"b\\\\c\"");
    }

    #[test]
    fn keystone_plan_is_the_set_the_issue_asks_for() {
        let ids: Vec<String> = keystone_steps().iter().map(step_id).collect();
        assert_eq!(
            ids,
            vec!["click", "type", "cmd-a", "cmd-c", "cmd-v", "release"]
        );
    }

    #[test]
    fn delivery_only_verdict_refuses_to_claim_an_input_was_applied() {
        let driven = healthy_driven();
        let received = vec!["pointer".to_string(), "key".to_string()];
        match evaluate_delivery_only(&driven, &received, true) {
            RcVerdict::Pass(detail) => {
                assert!(detail.contains("DELIVERY only"), "{detail}");
                assert!(detail.contains("cannot inject OS input"), "{detail}");
            }
            other => panic!("expected Pass, got {other:?}"),
        }
    }

    #[test]
    fn delivery_only_fails_when_the_peer_never_saw_a_published_kind() {
        let driven = healthy_driven();
        let received = vec!["pointer".to_string()];
        match evaluate_delivery_only(&driven, &received, true) {
            RcVerdict::TestFail(detail) => assert!(detail.contains("never received"), "{detail}"),
            other => panic!("expected TestFail, got {other:?}"),
        }
    }

    #[test]
    fn delivery_only_fails_when_the_web_peer_never_granted() {
        let driven = healthy_driven();
        match evaluate_delivery_only(&driven, &[], false) {
            RcVerdict::TestFail(detail) => assert!(detail.contains("never granted"), "{detail}"),
            other => panic!("expected TestFail, got {other:?}"),
        }
    }
}
