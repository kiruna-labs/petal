//! Whether the model may act on the shared window, and with what (#658).
//!
//! Pure policy: no native calls, no I/O, so every decision is unit-testable on
//! any platform. Execution lives elsewhere and re-checks these decisions at the
//! moment it acts — a grant is not a licence held over time.
//!
//! ## Threat model, stated plainly
//!
//! Both inputs to the model are attacker-influenceable. The shared window can
//! display text that reads as instructions ("click Compose, type …"), and any
//! participant can say the same thing out loud into push-to-talk. Prompt
//! injection is therefore not a hypothetical here, and no prompt wording
//! prevents it. This module's job is to make the blast radius small and every
//! action human-visible — not to pretend injection cannot happen.
//!
//! The consequences of that stance:
//! - **Per-action approval is the default.** Session-wide grant exists, but a
//!   human must opt into it explicitly; it is not what you get by saying yes
//!   once.
//! - **Fail closed everywhere.** An unknown application, an unreadable focused
//!   element, an unavailable takeover detector — each denies. A check whose
//!   answer we cannot obtain is a denial, never a default-allow.
//! - **The blocklist covers editors, not just terminals.** A command palette
//!   or an integrated terminal reaches a shell just as surely as Terminal.app.

use serde::{Deserialize, Serialize};

/// Longest text the model may type in one action.
pub const MAX_TEXT_LEN: usize = 2000;
/// Scroll bounds, in lines.
pub const MIN_SCROLL: i64 = 1;
pub const MAX_SCROLL: i64 = 100;

/// The keys the model may press. Deliberately navigation-only: an arbitrary
/// chord is a different capability entirely (think Cmd-Q, Cmd-W, Ctrl-C), and
/// there is no use case here that needs one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SafeKey {
    Return,
    Tab,
    Escape,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
}

impl SafeKey {
    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "Return" => SafeKey::Return,
            "Tab" => SafeKey::Tab,
            "Escape" => SafeKey::Escape,
            "ArrowUp" => SafeKey::ArrowUp,
            "ArrowDown" => SafeKey::ArrowDown,
            "ArrowLeft" => SafeKey::ArrowLeft,
            "ArrowRight" => SafeKey::ArrowRight,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScrollDirection {
    Up,
    Down,
    Left,
    Right,
}

impl ScrollDirection {
    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "up" => ScrollDirection::Up,
            "down" => ScrollDirection::Down,
            "left" => ScrollDirection::Left,
            "right" => ScrollDirection::Right,
            _ => return None,
        })
    }
}

/// A validated action the model asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Type into the focused field.
    Type(String),
    /// Click the element at `element_index` within accessibility snapshot
    /// `generation`. Citing the generation is what lets a stale reference be
    /// DETECTED rather than silently resolved to whatever now sits at that
    /// index.
    Click { generation: u64, element_index: usize },
    PressKey(SafeKey),
    Scroll { direction: ScrollDirection, amount: i64 },
}

impl Action {
    /// Whether this action additionally requires a working takeover detector.
    ///
    /// Typing into a field the human just verified is focused is materially
    /// safer than a click, which can land anywhere and cannot be previewed as
    /// precisely. The higher tier therefore also demands that we would notice
    /// the human taking back control.
    pub fn requires_takeover_detection(&self) -> bool {
        !matches!(self, Action::Type(_))
    }
}

/// Why an action's arguments were refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgError {
    UnknownTool,
    MissingArgument,
    TextTooLong,
    /// Text contained a control or format character. See [`validate_text`].
    ControlCharacter,
    KeyNotAllowed,
    DirectionNotAllowed,
    ScrollOutOfRange,
}

/// Validate text the model wants typed.
///
/// **Control and format characters are rejected outright, and this is
/// load-bearing rather than fastidious.** Text-injection backends special-case
/// a leading tab or newline in each chunk by synthesizing a real keypress. Bidi
/// overrides and other Unicode `Cf` characters can instead make the approval
/// card display a different-looking string from the one replay will type.
pub fn validate_text(text: &str) -> Result<String, ArgError> {
    if text.chars().count() > MAX_TEXT_LEN {
        return Err(ArgError::TextTooLong);
    }
    if text
        .chars()
        .any(|c| c.is_control() || is_unicode_format_character(c))
    {
        return Err(ArgError::ControlCharacter);
    }
    Ok(text.to_string())
}

/// Unicode General_Category=Cf ranges (Unicode 16). Kept explicit instead of
/// adding a Unicode database dependency for one security boundary.
///
/// `pub(crate)` because `ax_digest.rs` reuses this exact table: AX-tree text
/// read from a shared window needs the identical bidi/invisible-character
/// filter before it reaches either the model's context or a human-facing
/// approval card (`DigestIndex.title` -> `control_gate::describe_element`) --
/// two independent spoofing surfaces this module's own header comment
/// already treats as one threat.
pub(crate) fn is_unicode_format_character(c: char) -> bool {
    matches!(
        c as u32,
        0x00AD
            | 0x0600..=0x0605
            | 0x061C
            | 0x06DD
            | 0x070F
            | 0x0890..=0x0891
            | 0x08E2
            | 0x180E
            | 0x200B..=0x200F
            | 0x202A..=0x202E
            | 0x2060..=0x2064
            | 0x2066..=0x206F
            | 0xFEFF
            | 0xFFF9..=0xFFFB
            | 0x110BD
            | 0x110CD
            | 0x13430..=0x1343F
            | 0x1BCA0..=0x1BCA3
            | 0x1D173..=0x1D17A
            | 0xE0001
            | 0xE0020..=0xE007F
    )
}

/// Parse a tool call into a validated action.
pub fn parse_action(tool: &str, args: &serde_json::Value) -> Result<Action, ArgError> {
    match tool {
        "window_type" | "send_to_window" => {
            let text = args.get("text").and_then(|v| v.as_str()).ok_or(ArgError::MissingArgument)?;
            Ok(Action::Type(validate_text(text)?))
        }
        "window_click" => {
            let generation = args
                .get("generation")
                .and_then(|v| v.as_u64())
                .ok_or(ArgError::MissingArgument)?;
            let element_index = args
                .get("element_index")
                .and_then(|v| v.as_u64())
                .ok_or(ArgError::MissingArgument)? as usize;
            Ok(Action::Click {
                generation,
                element_index,
            })
        }
        "window_press_key" => {
            let key = args.get("key").and_then(|v| v.as_str()).ok_or(ArgError::MissingArgument)?;
            SafeKey::parse(key).map(Action::PressKey).ok_or(ArgError::KeyNotAllowed)
        }
        "window_scroll" => {
            let direction = args
                .get("direction")
                .and_then(|v| v.as_str())
                .ok_or(ArgError::MissingArgument)?;
            let direction = ScrollDirection::parse(direction).ok_or(ArgError::DirectionNotAllowed)?;
            let amount = args
                .get("amount")
                .and_then(|v| v.as_i64())
                .ok_or(ArgError::MissingArgument)?;
            if !(MIN_SCROLL..=MAX_SCROLL).contains(&amount) {
                return Err(ArgError::ScrollOutOfRange);
            }
            Ok(Action::Scroll { direction, amount })
        }
        _ => Err(ArgError::UnknownTool),
    }
}

/// Is the frontmost application one the model must never drive?
///
/// Exact bundle-id match, and **an unknown id is not allowed** — the caller
/// supplies `None` when it cannot resolve one, and that denies.
///
/// Editors are on this list alongside terminals: a command palette or an
/// integrated terminal reaches a shell exactly as a terminal emulator does, so
/// excluding only Terminal.app would be security theatre. Browsers are
/// deliberately NOT blocked — they are the main thing people share — which is
/// precisely why per-action approval is the default rather than an option.
pub fn blocklist_reason(bundle_id: Option<&str>) -> Option<&'static str> {
    let Some(id) = bundle_id else {
        return Some("unknown_target_application");
    };
    const PASSWORD_MANAGERS: &[&str] = &[
        "com.1password.1password",
        "com.agilebits.onepassword7",
        "com.bitwarden.desktop",
        "org.keepassxc.keepassxc",
        "com.apple.keychainaccess",
    ];
    const SYSTEM_UI: &[&str] = &[
        "com.apple.systempreferences",
        "com.apple.SecurityAgent",
        "com.apple.loginwindow",
    ];
    const TERMINALS: &[&str] = &[
        "com.apple.Terminal",
        "com.googlecode.iterm2",
        "dev.warp.Warp-Stable",
        "io.alacritty",
        "net.kovidgoyal.kitty",
        "com.mitchellh.ghostty",
        "com.github.wez.wezterm",
    ];
    // Integrated terminals and command palettes make these equivalent to a
    // shell for this purpose.
    const EDITORS: &[&str] = &[
        "com.microsoft.VSCode",
        "com.microsoft.VSCodeInsiders",
        "com.todesktop.230313mzl4w4u92", // Cursor
        "dev.zed.Zed",
        "com.sublimetext.4",
        "com.jetbrains.intellij",
        "com.jetbrains.pycharm",
        "com.jetbrains.WebStorm",
        "com.google.android.studio",
        "com.apple.dt.Xcode",
    ];
    const REMOTE_AND_VMS: &[&str] = &[
        "com.apple.ScreenSharing",
        "com.realvnc.vncviewer",
        "com.realvnc.vncserver",
        "com.edovia.screens5",
        "com.parallels.desktop.console",
        "com.utmapp.UTM",
    ];
    const SELF: &[&str] = &["com.petal.app"];

    if PASSWORD_MANAGERS.contains(&id) {
        return Some("blocked_password_manager");
    }
    if SYSTEM_UI.contains(&id) {
        return Some("blocked_system_ui");
    }
    if TERMINALS.contains(&id) {
        return Some("blocked_terminal");
    }
    if EDITORS.contains(&id) {
        return Some("blocked_editor");
    }
    if REMOTE_AND_VMS.contains(&id) {
        return Some("blocked_remote_desktop");
    }
    if SELF.contains(&id) {
        return Some("blocked_self");
    }
    None
}

/// Whether macOS secure input is active (a password field somewhere has taken
/// the keyboard). Modelled as a tri-state because "we could not tell" must
/// deny, not default to inactive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecureInput {
    Inactive,
    Active,
    Unknown,
}

/// Everything the gate needs to decide. Assembled by the caller immediately
/// before it acts, never cached.
#[derive(Debug, Clone)]
pub struct GrantContext<'a> {
    /// The window still exists and is still a live publication of ours.
    pub window_present: bool,
    /// Frontmost app's bundle id, or None if unresolvable.
    pub bundle_id: Option<&'a str>,
    pub secure_input: SecureInput,
    /// Whether the physical-takeover detector is currently working. When it is
    /// not, higher-risk actions must not be granted — we would be unable to
    /// notice the human taking back control.
    pub takeover_detection_healthy: bool,
    /// The user's master remote-control switch. AI control never exceeds what
    /// human remote control is allowed to do.
    pub remote_control_allowed: bool,
    /// The AI chat master switch.
    pub ai_chat_enabled: bool,
}

/// The gate's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Show the human an approval card for this action.
    AskUser,
    /// Refuse without troubling the human: nothing they could approve would
    /// make this safe, so a prompt would only train them to click through.
    Refuse { code: &'static str },
}

/// Decide whether an action may even be offered for approval. Fail-closed: any
/// unknown or unavailable input refuses.
pub fn grant_decision(action: &Action, ctx: &GrantContext<'_>) -> Decision {
    if !ctx.ai_chat_enabled {
        return Decision::Refuse { code: "ai_chat_disabled" };
    }
    if !ctx.remote_control_allowed {
        return Decision::Refuse { code: "remote_control_disabled" };
    }
    if !ctx.window_present {
        return Decision::Refuse { code: "window_unavailable" };
    }
    if let Some(code) = blocklist_reason(ctx.bundle_id) {
        return Decision::Refuse { code };
    }
    match ctx.secure_input {
        SecureInput::Inactive => {}
        // Unknown denies: a password field may hold the keyboard right now.
        SecureInput::Active | SecureInput::Unknown => {
            return Decision::Refuse { code: "secure_input_active" }
        }
    }
    if action.requires_takeover_detection() && !ctx.takeover_detection_healthy {
        return Decision::Refuse { code: "input_tap_unavailable" };
    }
    Decision::AskUser
}

/// How far an approval extends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GrantScope {
    /// This action only. The default, and what the approval card offers first.
    Once,
    /// Every subsequent action this session. An explicit escalation the human
    /// has to choose.
    Session,
}

/// Standing authorization carried across a session.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum Standing {
    /// Nothing granted yet — each action raises a card.
    #[default]
    None,
    /// The human granted the whole session.
    Session,
    /// The human refused; refusal is sticky until they deliberately re-grant,
    /// so a model that keeps asking cannot wear them down.
    Refused,
}

/// Does this action need a fresh approval card, given standing authorization?
pub fn needs_approval(standing: &Standing) -> bool {
    match standing {
        Standing::Session => false,
        Standing::None => true,
        // A refused session should not be raising cards at all; the caller
        // answers the model directly with `control_rejected`.
        Standing::Refused => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ok_ctx() -> GrantContext<'static> {
        GrantContext {
            window_present: true,
            bundle_id: Some("com.apple.TextEdit"),
            secure_input: SecureInput::Inactive,
            takeover_detection_healthy: true,
            remote_control_allowed: true,
            ai_chat_enabled: true,
        }
    }

    #[test]
    fn text_with_control_characters_is_refused() {
        // The whole point: a leading tab would be replayed as a real Tab press,
        // moving focus off the approved field before the rest is typed.
        assert_eq!(validate_text("\tsecret"), Err(ArgError::ControlCharacter));
        assert_eq!(validate_text("line\nbreak"), Err(ArgError::ControlCharacter));
        assert_eq!(validate_text("carriage\rreturn"), Err(ArgError::ControlCharacter));
        assert_eq!(validate_text("nul\0byte"), Err(ArgError::ControlCharacter));
        // Ordinary text, including non-ASCII, is fine.
        assert_eq!(validate_text("héllo wörld 👋").unwrap(), "héllo wörld 👋");
    }

    #[test]
    fn unicode_format_characters_cannot_spoof_the_approval_card() {
        // The exact production entry point: parse_action calls validate_text
        // before an Action (and therefore an approval card) can exist.
        for format in [
            '\u{00ad}', // soft hyphen
            '\u{200b}', // zero-width space
            '\u{200f}', // right-to-left mark
            '\u{202e}', // right-to-left override
            '\u{2066}', // left-to-right isolate
            '\u{2069}', // pop directional isolate
            '\u{feff}', // zero-width no-break space / BOM
            '\u{e007f}', // cancel tag
        ] {
            let text = format!("Allow harmless action {format}txt.exe");
            assert_eq!(
                parse_action("window_type", &json!({ "text": text })),
                Err(ArgError::ControlCharacter),
                "format character U+{:04X} reached an approval-card action",
                format as u32
            );
        }
        assert_eq!(
            parse_action("window_type", &json!({ "text": "مرحبا بالعالم" })),
            Ok(Action::Type("مرحبا بالعالم".into())),
            "ordinary RTL letters are not format controls"
        );
    }

    #[test]
    fn overlong_text_is_refused_by_character_count_not_bytes() {
        let long: String = "é".repeat(MAX_TEXT_LEN + 1);
        assert_eq!(validate_text(&long), Err(ArgError::TextTooLong));
        let exact: String = "é".repeat(MAX_TEXT_LEN);
        assert!(validate_text(&exact).is_ok());
    }

    #[test]
    fn only_navigation_keys_parse() {
        assert_eq!(
            parse_action("window_press_key", &json!({"key": "Return"})),
            Ok(Action::PressKey(SafeKey::Return))
        );
        // No arbitrary chords, no single letters, no modifiers.
        for bad in ["a", "Cmd+Q", "F1", "Delete", "Backspace", ""] {
            assert_eq!(
                parse_action("window_press_key", &json!({ "key": bad })),
                Err(ArgError::KeyNotAllowed),
                "{bad} should not be allowed"
            );
        }
    }

    #[test]
    fn scroll_is_bounded() {
        assert!(parse_action("window_scroll", &json!({"direction":"down","amount":10})).is_ok());
        for bad in [0, -5, 101, 100_000] {
            assert_eq!(
                parse_action("window_scroll", &json!({"direction":"down","amount":bad})),
                Err(ArgError::ScrollOutOfRange),
                "amount {bad}"
            );
        }
        assert_eq!(
            parse_action("window_scroll", &json!({"direction":"sideways","amount":5})),
            Err(ArgError::DirectionNotAllowed)
        );
    }

    #[test]
    fn unknown_tools_and_missing_args_refuse() {
        assert_eq!(parse_action("rm_rf", &json!({})), Err(ArgError::UnknownTool));
        assert_eq!(
            parse_action("window_type", &json!({})),
            Err(ArgError::MissingArgument)
        );
        assert_eq!(
            parse_action("window_click", &json!({"generation": 1})),
            Err(ArgError::MissingArgument)
        );
    }

    #[test]
    fn click_carries_the_generation_it_was_derived_from() {
        // Without the generation a stale index silently resolves to whatever
        // now occupies that slot — clicking Delete where Cancel used to be.
        assert_eq!(
            parse_action("window_click", &json!({"generation": 4, "element_index": 9})),
            Ok(Action::Click {
                generation: 4,
                element_index: 9
            })
        );
    }

    #[test]
    fn typing_is_a_lower_tier_than_clicking() {
        assert!(!Action::Type("hi".into()).requires_takeover_detection());
        assert!(Action::Click {
            generation: 1,
            element_index: 0
        }
        .requires_takeover_detection());
        assert!(Action::PressKey(SafeKey::Return).requires_takeover_detection());
        assert!(Action::Scroll {
            direction: ScrollDirection::Down,
            amount: 3
        }
        .requires_takeover_detection());
    }

    #[test]
    fn an_unresolvable_application_denies() {
        // Fail closed: not knowing what we are driving is a denial.
        assert_eq!(blocklist_reason(None), Some("unknown_target_application"));
    }

    #[test]
    fn terminals_editors_password_managers_and_system_ui_are_blocked() {
        for (id, expected) in [
            ("com.apple.Terminal", "blocked_terminal"),
            ("com.googlecode.iterm2", "blocked_terminal"),
            ("com.mitchellh.ghostty", "blocked_terminal"),
            // Editors matter as much as terminals: integrated terminals and
            // command palettes reach a shell just as well.
            ("com.microsoft.VSCode", "blocked_editor"),
            ("dev.zed.Zed", "blocked_editor"),
            ("com.todesktop.230313mzl4w4u92", "blocked_editor"),
            ("com.1password.1password", "blocked_password_manager"),
            ("com.apple.keychainaccess", "blocked_password_manager"),
            ("com.apple.systempreferences", "blocked_system_ui"),
            ("com.apple.SecurityAgent", "blocked_system_ui"),
            ("com.apple.ScreenSharing", "blocked_remote_desktop"),
            ("com.petal.app", "blocked_self"),
        ] {
            assert_eq!(blocklist_reason(Some(id)), Some(expected), "{id}");
        }
    }

    #[test]
    fn ordinary_apps_and_browsers_are_not_blocked() {
        // Browsers are the main thing people share; blocking them would gut
        // the feature. Per-action approval is the mitigation instead.
        for id in [
            "com.apple.TextEdit",
            "com.apple.Safari",
            "com.google.Chrome",
            "com.figma.Desktop",
        ] {
            assert_eq!(blocklist_reason(Some(id)), None, "{id}");
        }
    }

    #[test]
    fn secure_input_unknown_denies_like_active() {
        // "We could not tell" must never mean "probably fine".
        for state in [SecureInput::Active, SecureInput::Unknown] {
            let ctx = GrantContext {
                secure_input: state,
                ..ok_ctx()
            };
            assert_eq!(
                grant_decision(&Action::Type("hi".into()), &ctx),
                Decision::Refuse {
                    code: "secure_input_active"
                },
                "{state:?}"
            );
        }
    }

    #[test]
    fn a_broken_takeover_detector_blocks_only_the_higher_tier() {
        let ctx = GrantContext {
            takeover_detection_healthy: false,
            ..ok_ctx()
        };
        // Clicking without being able to notice the human take over: refused.
        assert_eq!(
            grant_decision(
                &Action::Click {
                    generation: 1,
                    element_index: 0
                },
                &ctx
            ),
            Decision::Refuse {
                code: "input_tap_unavailable"
            }
        );
        // Typing into an already-verified focused field stays available.
        assert_eq!(
            grant_decision(&Action::Type("hi".into()), &ctx),
            Decision::AskUser
        );
    }

    #[test]
    fn master_switches_win_over_everything() {
        for (ctx, code) in [
            (
                GrantContext {
                    ai_chat_enabled: false,
                    ..ok_ctx()
                },
                "ai_chat_disabled",
            ),
            (
                GrantContext {
                    remote_control_allowed: false,
                    ..ok_ctx()
                },
                "remote_control_disabled",
            ),
        ] {
            assert_eq!(
                grant_decision(&Action::Type("hi".into()), &ctx),
                Decision::Refuse { code }
            );
        }
    }

    #[test]
    fn a_vanished_window_denies() {
        let ctx = GrantContext {
            window_present: false,
            ..ok_ctx()
        };
        assert_eq!(
            grant_decision(&Action::Type("hi".into()), &ctx),
            Decision::Refuse {
                code: "window_unavailable"
            }
        );
    }

    #[test]
    fn a_clean_context_asks_the_human_rather_than_acting() {
        // The default is a card, never silent execution.
        assert_eq!(
            grant_decision(&Action::Type("hello".into()), &ok_ctx()),
            Decision::AskUser
        );
    }

    #[test]
    fn approval_is_per_action_until_the_human_escalates() {
        assert!(needs_approval(&Standing::None));
        assert!(!needs_approval(&Standing::Session));
        // Refusal is sticky: a model that keeps asking must not wear the user
        // down by repetition.
        assert!(needs_approval(&Standing::Refused));
    }

    #[test]
    fn grant_scope_defaults_are_explicit_on_the_wire() {
        assert_eq!(serde_json::to_string(&GrantScope::Once).unwrap(), "\"once\"");
        assert_eq!(
            serde_json::to_string(&GrantScope::Session).unwrap(),
            "\"session\""
        );
    }
}
