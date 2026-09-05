//! `petal://join/<access-code>` deep-link handling (issue #2/#160).
//!
//! Split in two:
//! - [`parse_join_deep_link`] -- pure, unit-tested URL parsing. Accepts the
//!   native compatibility join shape (`petal://join/<url-encoded-access-code>`),
//!   rejects everything else, and resolves the access code to the internal
//!   room credential (never accepts the raw internal credential itself --
//!   the access code is the only user-facing join input, same as the web
//!   invite link; see #42).
//! - [`handle_deep_link_urls`] -- side-effectful handler wired to the
//!   deep-link plugin's `on_open_url`/`get_current` in `lib.rs`. It only
//!   NAVIGATES the main webview to `/meeting/<credential>` and shows/focuses the
//!   main window -- it deliberately does NOT call `session::join_room` from
//!   Rust: the meeting route's own `onMount` performs the real (idempotent)
//!   join with the frontend's persisted identity/display name, so
//!   identity/presence wiring stays in exactly one place.
//!
//! Platform reality (per the official plugin docs, recorded in issue #2
//! as a user-accepted limitation): on macOS the `petal:` scheme is only
//! registered with LaunchServices via a bundled `.app`'s Info.plist --
//! runtime registration is impossible -- so `open "petal://join/..."` only
//! reaches this handler from an installed bundle, never the `tauri dev`
//! binary. The handler logic itself is testable in dev by invoking it with a
//! test URL.

use percent_encoding::{percent_decode_str, utf8_percent_encode, NON_ALPHANUMERIC};
use tauri::Manager;

use crate::rooms::internal_credential_for_access_code;

/// Parse a `petal://join/<access-code>` invite link into the canonical
/// internal room credential.
///
/// Returns `None` for anything that isn't exactly this shape: wrong scheme,
/// wrong action ("host" segment), missing/empty code, label-only room,
/// malformed access code, or an un-encoded `/` extra path segment (the Invite
/// button `encodeURIComponent`s the code, so a literal extra path
/// segment is malformed). Scheme and action are matched
/// ASCII-case-insensitively (URL schemes/hosts are case-insensitive); an
/// optional trailing `/` and any query/fragment tail are tolerated and
/// stripped.
pub fn parse_join_deep_link(url: &str) -> Option<String> {
    parse_join_deep_link_parts(url).map(|(credential, _)| credential)
}

fn parse_join_deep_link_parts(url: &str) -> Option<(String, String)> {
    let rest = strip_prefix_ascii_ci(url.trim(), "petal://")?;
    let rest = strip_prefix_ascii_ci(rest, "join/")?;
    // Drop any query/fragment; tolerate one trailing slash.
    let encoded = rest.split(['?', '#']).next().unwrap_or("");
    let encoded = encoded.strip_suffix('/').unwrap_or(encoded);
    if encoded.is_empty() || encoded.contains('/') {
        return None;
    }
    let decoded = percent_decode_str(encoded).decode_utf8().ok()?;
    let access_code = crate::rooms::normalize_access_code(decoded.as_ref())?;
    let credential = internal_credential_for_access_code(&access_code)?;
    Some((credential, access_code))
}

/// ASCII-case-insensitive `strip_prefix`. `prefix` must be plain ASCII
/// (true for both callers above); the `is_char_boundary` guard makes the
/// slice safe even when `s` starts with multi-byte characters.
fn strip_prefix_ascii_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() >= prefix.len()
        && s.is_char_boundary(prefix.len())
        && s[..prefix.len()].eq_ignore_ascii_case(prefix)
    {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

/// Handle a batch of deep-link URLs (from `on_open_url` or the launch-time
/// `get_current`). Recognized `petal://join/<credential>` links navigate the
/// main webview to `/meeting/<credential>`; unrecognized URLs are logged and
/// ignored.
pub fn handle_deep_link_urls(app: &tauri::AppHandle, urls: Vec<String>) {
    for url in urls {
        match parse_join_deep_link_parts(&url) {
            Some((credential, access_code)) => {
                log::info!(
                    "deep-link: join link received for room '{}'",
                    room_label_for_log(&credential)
                );
                // Persist before navigation so the meeting route and the next
                // launch both retain the real bearer code from the link.
                if let Some(rooms) = app.try_state::<crate::rooms::RoomsState>() {
                    if let Err(e) = rooms.create(&access_code, true) {
                        log::warn!("deep-link: failed to persist joined room: {e}");
                    }
                }
                navigate_to_meeting(app, &credential);
            }
            None => {
                let _ = url;
                log::warn!("deep-link: ignoring unrecognized URL");
            }
        }
    }
}

fn room_label_for_log(credential: &str) -> &str {
    credential
        .rsplit_once('-')
        .map(|(label, _)| label)
        .unwrap_or("unknown")
}

/// Navigate the main webview to `/meeting/<credential>` and show/focus the main
/// window. The route's own `onMount` then performs the real `join_room`.
///
/// Retries briefly if the main window doesn't exist yet: when the app is
/// LAUNCHED by a link click (rather than already running), `get_current`
/// fires from `setup()` before the config-defined main window is
/// necessarily up.
///
/// The main window CAN be hidden mid-session again: the red traffic dot hides
/// it. `set_focus` is a no-op on a hidden macOS window (tao checks
/// `is_visible` first), so joining from a link while hidden would put the user
/// in the room -- mic live -- with no window at all. Show explicitly, gated on
/// the reveal having already happened so #636's cold start is untouched.
fn navigate_to_meeting(app: &tauri::AppHandle, credential: &str) {
    use tauri::Manager;

    let app = app.clone();
    let path = format!(
        "/meeting/{}",
        utf8_percent_encode(credential, NON_ALPHANUMERIC)
    );
    let room_label = room_label_for_log(credential).to_string();
    tauri::async_runtime::spawn(async move {
        for _attempt in 0..25 {
            if let Some(window) = app.get_webview_window("main") {
                // A hard `location.assign` (not a SvelteKit `goto`, which
                // isn't reachable from Rust) works in both hosting modes:
                // vite dev serves the SPA for any path, and the bundled
                // app's asset resolver falls back to index.html
                // (adapter-static `fallback: "index.html"`), after which
                // the client router resolves /meeting/<room>. JSON-encode
                // the path so it's a safe JS string literal.
                let js = format!(
                    "window.location.assign({});",
                    serde_json::to_string(&path).unwrap_or_else(|_| "\"/main\"".into())
                );
                if let Err(e) = window.eval(js.as_str()) {
                    log::warn!("deep-link: failed to navigate main webview: {e}");
                } else {
                    log::info!("deep-link: navigated main webview to room '{room_label}'");
                }
                // No UNCONDITIONAL `window.show()` (#636). On a cold
                // deep-link launch this runs within ~200ms, long before the
                // frontend has mounted, so showing would put the unpainted
                // black square-cornered window on screen -- the exact bug the
                // reveal gate exists to prevent, on the coldest launch path
                // there is. The navigation above triggers a fresh mount, whose
                // `frontend_ready` reveals the window with content already in
                // it.
                //
                // But once the reveal HAS happened, an off-screen main window
                // was hidden by the user (red traffic dot), and
                // `frontend_ready` cannot save us: `reveal_main_window` is a
                // spent one-shot by then. Without this the user joins the
                // meeting with a live mic and no visible UI.
                if crate::main_window_revealed() {
                    crate::show_and_activate_main_window(&app, "deep-link");
                }
                let _ = window.unminimize();
                let _ = window.set_focus();
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        log::warn!("deep-link: main window never appeared; dropping meeting navigation");
    });
}

#[cfg(test)]
mod tests {
    use super::parse_join_deep_link;
    use super::room_label_for_log;
    use crate::rooms::internal_credential_for_access_code;

    #[test]
    fn parses_a_full_access_code() {
        let credential = internal_credential_for_access_code("abc-defg-hjk").unwrap();
        assert_eq!(
            parse_join_deep_link("petal://join/abc-defg-hjk"),
            Some(credential)
        );
    }

    #[test]
    fn canonicalizes_uppercase_access_codes() {
        let credential = internal_credential_for_access_code("abc-defg-hjk").unwrap();
        assert_eq!(
            parse_join_deep_link("petal://join/ABC-DEFG-HJK"),
            Some(credential)
        );
    }

    #[test]
    fn decodes_percent_encoded_codes_before_validation() {
        let credential = internal_credential_for_access_code("abc-defg-hjk").unwrap();
        assert_eq!(
            parse_join_deep_link("petal://join/abc%2Ddefg-hjk"),
            Some(credential)
        );
    }

    #[test]
    fn scheme_and_action_are_case_insensitive_and_whitespace_tolerant() {
        let credential = internal_credential_for_access_code("abc-defg-hjk").unwrap();
        assert_eq!(
            parse_join_deep_link("  PETAL://Join/abc-defg-hjk  "),
            Some(credential)
        );
    }

    #[test]
    fn tolerates_trailing_slash_query_and_fragment() {
        let credential = internal_credential_for_access_code("abc-defg-hjk").unwrap();
        assert_eq!(
            parse_join_deep_link("petal://join/abc-defg-hjk/"),
            Some(credential.clone())
        );
        assert_eq!(
            parse_join_deep_link("petal://join/abc-defg-hjk?utm=x#frag"),
            Some(credential)
        );
    }

    #[test]
    fn rejects_wrong_scheme() {
        let credential = "eng-sync-0123456789abcdef0123456789abcdef";
        assert_eq!(
            parse_join_deep_link(&format!("https://join/{credential}")),
            None
        );
        assert_eq!(
            parse_join_deep_link(&format!("petalx://join/{credential}")),
            None
        );
        assert_eq!(
            parse_join_deep_link(&format!("petal:/join/{credential}")),
            None
        );
    }

    #[test]
    fn rejects_label_only_links() {
        assert_eq!(parse_join_deep_link("petal://join/eng-sync"), None);
        assert_eq!(parse_join_deep_link("petal://join/Design%20Review"), None);
    }

    #[test]
    fn rejects_malformed_links() {
        assert_eq!(parse_join_deep_link(""), None);
        assert_eq!(parse_join_deep_link("petal://"), None);
        assert_eq!(parse_join_deep_link("petal://join"), None);
        assert_eq!(parse_join_deep_link("petal://join/"), None);
        assert_eq!(parse_join_deep_link("petal://join//"), None);
        assert_eq!(parse_join_deep_link("petal://other/eng-sync"), None);
        // Un-encoded extra path segment is not a credential.
        assert_eq!(
            parse_join_deep_link("petal://join/a-0123456789abcdef0123456789abcdef/b"),
            None
        );
        // Decoded value is not a credential.
        assert_eq!(
            parse_join_deep_link("petal://join/a%2Fb-0123456789abcdef0123456789abcdef"),
            None
        );
        assert_eq!(parse_join_deep_link("petal://join/%20%20"), None);
        // Invalid UTF-8 after decoding.
        assert_eq!(parse_join_deep_link("petal://join/%FF"), None);
    }

    #[test]
    fn log_label_does_not_include_credential_suffix() {
        assert_eq!(
            room_label_for_log("eng-sync-0123456789abcdef0123456789abcdef"),
            "eng-sync"
        );
    }
}
