//! AI chat session state + error taxonomy — pure, unit-tested, no I/O.
//!
//! #656 requires that every failure reaches the user as a distinct, legible
//! state rather than a silent dead button, and #657 puts these same values on
//! the wire (`petal.ai-chat`), so the vocabulary is pinned here once and shared
//! by the engine, the UI, and the data-channel messages. Freeform error strings
//! are deliberately not representable.

use serde::{Deserialize, Serialize};

/// Why a session ended (or refused to start). Serialized kebab-case; these are
/// the exact tokens that appear in UI state and on the `petal.ai-chat` wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EndReason {
    /// A participant pressed stop.
    Stopped,
    /// Session cap reached, or the server signalled `goAway`. A NORMAL end —
    /// the UI must render this as "time limit", never as an error.
    TimeLimit,
    /// The sharer has AI chat turned off.
    Disabled,
    /// The requested window has no live publication owned by this client.
    /// Also covers a share stopping mid-session.
    NotShared,
    /// Another session already holds this sharer's single session slot.
    Busy,
    /// Token mint refused for rate limiting (HTTP 429).
    RateLimited,
    /// Hosted AI chat is switched off server-side (HTTP 503 kill switch).
    HostedUnavailable,
    /// The token mint could not be reached at all.
    Offline,
    /// Mint failed for a reason that is neither rate limit, kill switch, nor
    /// connectivity (bad response, auth rejected, …).
    MintFailed,
    /// The configured model is gone/renamed — BYOK users must update Petal.
    ModelUnavailable,
    /// The Gemini project is out of credits / over quota.
    Quota,
    /// Anything else. The specific cause is logged locally, never shown raw.
    Error,
}

impl EndReason {
    /// Whether this is an ordinary conclusion rather than a failure. The UI
    /// styles these two classes differently, and only failures are worth a
    /// prominent toast.
    pub fn is_normal(self) -> bool {
        matches!(self, EndReason::Stopped | EndReason::TimeLimit)
    }

    /// Short, user-facing sentence. Kept here so desktop and web-harness can
    /// render identical copy from the same token (#657 parity), and so no
    /// caller is tempted to invent its own wording.
    pub fn user_message(self) -> &'static str {
        match self {
            EndReason::Stopped => "AI chat ended.",
            EndReason::TimeLimit => "AI chat reached its time limit.",
            EndReason::Disabled => "AI chat is turned off for this window.",
            EndReason::NotShared => "That window is no longer being shared.",
            EndReason::Busy => "An AI chat is already running for this window.",
            EndReason::RateLimited => "Too many AI chat sessions just now. Try again shortly.",
            EndReason::HostedUnavailable => "AI chat is temporarily unavailable.",
            EndReason::Offline => "Could not reach the AI chat service.",
            EndReason::MintFailed => "Could not start AI chat.",
            EndReason::ModelUnavailable => "This AI model is unavailable — update Petal.",
            EndReason::Quota => "The AI chat quota for this key is used up.",
            EndReason::Error => "AI chat stopped unexpectedly.",
        }
    }
}

/// Lifecycle of one session, as seen by every UI surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "kebab-case")]
pub enum Phase {
    /// Token minted / WS dialling; not yet usable.
    Connecting,
    /// `setupComplete` received — push-to-talk is live.
    Live,
    /// Terminal.
    Ended { reason: EndReason },
}

impl Phase {
    pub fn is_terminal(self) -> bool {
        matches!(self, Phase::Ended { .. })
    }
}

/// Classify a token-mint HTTP status into the taxonomy. Pure so the mapping is
/// testable without a backend.
pub fn classify_mint_status(status: u16) -> EndReason {
    match status {
        429 => EndReason::RateLimited,
        503 => EndReason::HostedUnavailable,
        // 401/403 mean our LiveKit bearer was rejected or we are not a live
        // participant — not something the user can fix by retrying, and NOT a
        // kill switch, so it is a plain mint failure.
        _ => EndReason::MintFailed,
    }
}

/// Classify a Gemini WebSocket close reason. The server sends human-readable
/// prose, so match on the stable substrings rather than close codes (which it
/// reuses across very different causes).
///
/// Verified against live responses during the #654 spike: an out-of-credit
/// project closes with "Your prepayment credits are depleted…", and an
/// unusable credential closes with "API key not valid" / "unregistered
/// callers".
pub fn classify_close_reason(reason: &str) -> EndReason {
    let r = reason.to_lowercase();
    if r.contains("credit") || r.contains("prepayment") || r.contains("quota") || r.contains("billing")
    {
        return EndReason::Quota;
    }
    if r.contains("api key")
        || r.contains("unregistered")
        || r.contains("token")
        || r.contains("credential")
        || r.contains("permission")
    {
        return EndReason::MintFailed;
    }
    if r.contains("not found") || r.contains("model") {
        return EndReason::ModelUnavailable;
    }
    EndReason::Error
}

/// Remaining seconds for the countdown, saturating at zero. Pure arithmetic,
/// separated because an off-by-one here shows up as a UI that counts past the
/// end or ends a second early.
pub fn seconds_left(cap_seconds: u64, elapsed_seconds: u64) -> u64 {
    cap_seconds.saturating_sub(elapsed_seconds)
}

/// What the session's end reason should be once the WS `Close` frame that
/// always follows a `goAway` finally arrives. If a `goAway` already told us
/// this is a normal time-limit end, the close frame's own reason (often
/// empty or generic right after a goAway) must NOT reclassify it — that was
/// a real bug: a correctly-detected graceful end silently became
/// `EndReason::Error` (a user-visible "stopped unexpectedly" toast) because
/// `classify_close_reason("")` falls through every pattern to `Error`.
pub fn resolve_close_end_reason(
    go_away_received: bool,
    close_reason: &str,
    reason_from_go_away: EndReason,
) -> EndReason {
    if go_away_received {
        reason_from_go_away
    } else {
        classify_close_reason(close_reason)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_ends_are_not_failures() {
        assert!(EndReason::Stopped.is_normal());
        assert!(EndReason::TimeLimit.is_normal());
        for reason in [
            EndReason::Disabled,
            EndReason::NotShared,
            EndReason::Busy,
            EndReason::RateLimited,
            EndReason::HostedUnavailable,
            EndReason::Offline,
            EndReason::MintFailed,
            EndReason::ModelUnavailable,
            EndReason::Quota,
            EndReason::Error,
        ] {
            assert!(!reason.is_normal(), "{reason:?} should not be normal");
        }
    }

    #[test]
    fn every_reason_has_distinct_user_copy() {
        let all = [
            EndReason::Stopped,
            EndReason::TimeLimit,
            EndReason::Disabled,
            EndReason::NotShared,
            EndReason::Busy,
            EndReason::RateLimited,
            EndReason::HostedUnavailable,
            EndReason::Offline,
            EndReason::MintFailed,
            EndReason::ModelUnavailable,
            EndReason::Quota,
            EndReason::Error,
        ];
        let mut seen = std::collections::HashSet::new();
        for reason in all {
            let msg = reason.user_message();
            assert!(!msg.is_empty(), "{reason:?} has empty copy");
            assert!(seen.insert(msg), "duplicate user copy for {reason:?}");
        }
    }

    #[test]
    fn mint_status_maps_rate_limit_and_kill_switch() {
        assert_eq!(classify_mint_status(429), EndReason::RateLimited);
        assert_eq!(classify_mint_status(503), EndReason::HostedUnavailable);
        assert_eq!(classify_mint_status(403), EndReason::MintFailed);
        assert_eq!(classify_mint_status(500), EndReason::MintFailed);
    }

    /// Regression test for the goAway->Close downgrade bug: `session.rs`
    /// used to unconditionally reclassify `end_reason` from the `Close`
    /// frame's own reason string even after a `goAway` had already marked
    /// the end as a normal time limit. Google's real close frame right
    /// after a goAway carries an empty/generic reason, which
    /// `classify_close_reason` falls through to `Error` -- silently
    /// downgrading a graceful, correctly-detected end into a user-visible
    /// "AI chat stopped unexpectedly" failure toast. Reverting
    /// `resolve_close_end_reason` to always call `classify_close_reason`
    /// (ignoring `go_away_received`) makes the first assertion below fail.
    #[test]
    fn go_away_reason_survives_the_close_frame_that_follows_it() {
        assert_eq!(
            resolve_close_end_reason(true, "", EndReason::TimeLimit),
            EndReason::TimeLimit,
            "a goAway's TimeLimit must not be downgraded by an empty close reason"
        );
        assert_eq!(
            resolve_close_end_reason(true, "connection reset", EndReason::TimeLimit),
            EndReason::TimeLimit,
            "a goAway's TimeLimit must not be downgraded by any close reason"
        );
        // Without a prior goAway, the close frame's own reason still governs
        // -- this fix must not blunt real close-reason classification.
        assert_eq!(
            resolve_close_end_reason(
                false,
                "Your prepayment credits are depleted. Please go to AI Studio",
                EndReason::Error
            ),
            EndReason::Quota,
            "close-reason classification must still work when there was no goAway"
        );
    }

    #[test]
    fn close_reason_classification_matches_live_observations() {
        // Verbatim prefix of what Google actually sent during the #654 spike.
        assert_eq!(
            classify_close_reason("Your prepayment credits are depleted. Please go to AI Studio"),
            EndReason::Quota
        );
        assert_eq!(
            classify_close_reason("API key not valid. Please pass a valid API key."),
            EndReason::MintFailed
        );
        assert_eq!(
            classify_close_reason(
                "Method doesn't allow unregistered callers (callers without established identity)"
            ),
            EndReason::MintFailed
        );
        assert_eq!(classify_close_reason("something else entirely"), EndReason::Error);
    }

    #[test]
    fn quota_wins_over_generic_error_text() {
        // A message mentioning both should classify as the actionable one.
        assert_eq!(
            classify_close_reason("internal error: billing account suspended"),
            EndReason::Quota
        );
    }

    #[test]
    fn countdown_saturates_and_never_wraps() {
        assert_eq!(seconds_left(300, 0), 300);
        assert_eq!(seconds_left(300, 299), 1);
        assert_eq!(seconds_left(300, 300), 0);
        // Overrun must clamp, not wrap to u64::MAX.
        assert_eq!(seconds_left(300, 5_000), 0);
    }

    #[test]
    fn phase_terminality() {
        assert!(!Phase::Connecting.is_terminal());
        assert!(!Phase::Live.is_terminal());
        assert!(Phase::Ended {
            reason: EndReason::Stopped
        }
        .is_terminal());
    }

    #[test]
    fn phase_serializes_with_kebab_tokens() {
        let json = serde_json::to_string(&Phase::Ended {
            reason: EndReason::TimeLimit,
        })
        .unwrap();
        assert!(json.contains("\"phase\":\"ended\""), "{json}");
        assert!(json.contains("\"reason\":\"time-limit\""), "{json}");
    }
}
