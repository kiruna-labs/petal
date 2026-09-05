//! `petal.ai-chat` — the room-wide AI chat data-channel contract (#657).
//!
//! One participant hosts the Gemini session (always the window's sharer, since
//! only they have the pixels and the accessibility tree); everyone else drives
//! and observes it through this topic.
//!
//! ## Authorization is per message kind, and it is not optional
//!
//! LiveKit tells us the *authenticated* sender of every packet, and Petal's
//! existing rule is that identity always comes from there, never from the
//! payload. That prevents forged attribution but says nothing about who may
//! send WHAT — so without the matrix below, any peer could broadcast a
//! `state` claiming a session is running, or forge `transcript` lines
//! attributed to the assistant on someone else's window.
//!
//! | message | accepted from |
//! |---|---|
//! | `startRequest` / `stopRequest` | any current participant; acted on only by the window's owner |
//! | `state`, `transcript` | the window owner ONLY |
//! | `pttStart` / `pttEnd` | the speaker themselves ONLY |
//!
//! [`authorize`] is pure and unit-tested precisely because it is the whole
//! security boundary of this topic.
//!
//! ## Floor control
//!
//! Manual-activity mode is a single serial audio stream: two speakers
//! interleaved do not mix, they corrupt the turn. So exactly one participant
//! holds the push-to-talk floor at a time and everyone else is told who has it.

use serde::{Deserialize, Serialize};

use super::state::EndReason;

pub const TOPIC: &str = "petal.ai-chat";
pub const VERSION: u8 = 1;

/// Track-name prefix for the assistant's published voice. Every surface must
/// exclude these from speaking-indicator and mic-mute logic — the assistant
/// is not the sharer, and muting your microphone must not mute the assistant.
pub const AI_TRACK_PREFIX: &str = "petal-ai-";

/// Build the assistant's audio track name for a window.
pub fn ai_track_name(window_id: u32) -> String {
    format!("{AI_TRACK_PREFIX}window-{window_id}")
}

/// Is this an assistant voice track (rather than a human participant's mic)?
pub fn is_ai_track(track_name: &str) -> bool {
    track_name.starts_with(AI_TRACK_PREFIX)
}

/// Identifies the shared window a message is about. A raw `CGWindowID` is only
/// unique on the machine that produced it, so the owner's identity is part of
/// the key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowKey {
    pub window_id: u32,
    pub owner_identity: String,
}

/// NOTE on the serde attributes: `rename_all` on an enum renames the VARIANTS
/// only — struct-variant FIELDS need `rename_all_fields`. Without it this type
/// silently emitted `started_by`/`seconds_left`/`active_speaker` while the
/// contract and the web client use camelCase, and nothing failed: unknown
/// fields are ignored on the way in, so a round-trip test still passed while
/// the two implementations disagreed on the wire. Found by the web-client
/// implementation, not by these tests — which is why the tests below now
/// assert the literal JSON KEY NAMES rather than just round-tripping.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Body {
    /// Ask the window's owner to start a session.
    StartRequest,
    /// Ask the window's owner to stop the session.
    StopRequest,
    /// Owner-authored session state. Also serves as the liveness heartbeat:
    /// receivers expire a session that stops being reported (see
    /// [`STATE_HEARTBEAT`]), so a host that crashes cannot leave the room
    /// showing a phantom live assistant.
    State {
        active: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        started_by: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        seconds_left: Option<u64>,
        /// Who currently holds the push-to-talk floor, if anyone.
        #[serde(skip_serializing_if = "Option::is_none")]
        active_speaker: Option<String>,
        /// Present when the session is not running for a reason worth showing.
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<EndReason>,
    },
    /// Claim the push-to-talk floor.
    PttStart,
    /// Release the push-to-talk floor.
    PttEnd,
    /// Send a typed turn into the session — the text equivalent of a
    /// push-to-talk utterance, but deliberately NOT a floor action: unlike
    /// voice, a typed message has no "who's speaking" ambiguity to arbitrate,
    /// so it never claims or touches the PTT floor and any participant may
    /// send one independent of who (if anyone) currently holds it.
    SendText { text: String },
    /// A transcript delta, authored by the owner.
    Transcript {
        role: TranscriptRole,
        text: String,
        /// `final` is a Rust keyword, so the field is `final_` and renamed
        /// explicitly. It is deliberately NOT `#[serde(default)]`: defaulting
        /// meant a payload whose key we had wrong still deserialized, quietly,
        /// as `false` — which is precisely how the camelCase break survived a
        /// round-trip test.
        #[serde(rename = "final")]
        final_: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TranscriptRole {
    User,
    Assistant,
}

/// How often the owner republishes `State` while a session is live.
pub const STATE_HEARTBEAT_SECONDS: u64 = 5;
/// Missed heartbeats before a receiver declares the session gone. Chosen so a
/// brief network hiccup does not clear the UI, but a crashed host does.
pub const STATE_MISSED_HEARTBEATS_BEFORE_STALE: u32 = 3;

/// A message as it travels on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub v: u8,
    #[serde(flatten)]
    pub key: WindowKey,
    #[serde(flatten)]
    pub body: Body,
}

/// Why a received message was rejected. Returned by [`authorize`] so the
/// caller can log a precise reason rather than dropping silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    /// Wrong protocol version.
    UnsupportedVersion,
    /// Only the window's owner may author this kind of message.
    NotWindowOwner,
    /// A participant may only speak for themselves.
    NotSelf,
}

/// The authorization matrix. `sender` is the AUTHENTICATED LiveKit identity of
/// the packet's sender — never a value taken from the payload.
///
/// `local_identity` is who we are; it is not consulted for authorization, only
/// supplied for symmetry with callers that filter their own echoes.
pub fn authorize(message: &Message, sender: &str) -> Result<(), Rejection> {
    if message.v != VERSION {
        return Err(Rejection::UnsupportedVersion);
    }
    match &message.body {
        // Anyone in the room may ASK. The owner decides whether to act, and
        // enforces its own preconditions (feature enabled, window shared).
        Body::StartRequest | Body::StopRequest => Ok(()),

        // Session truth and transcript may only come from the host, or a peer
        // could fake a running session or put words in the assistant's mouth.
        Body::State { .. } | Body::Transcript { .. } => {
            if sender == message.key.owner_identity {
                Ok(())
            } else {
                Err(Rejection::NotWindowOwner)
            }
        }

        // You may only claim or release the floor for yourself. Otherwise a
        // peer could make the host tap someone else's microphone.
        Body::PttStart | Body::PttEnd => {
            // The sender IS the speaker by construction: the host attributes
            // the floor to the authenticated sender, so there is nothing in the
            // payload to disagree with. Rejecting here is therefore about
            // shape, not content — kept explicit so a future payload-carried
            // speaker field cannot quietly become authoritative.
            if sender.is_empty() {
                Err(Rejection::NotSelf)
            } else {
                Ok(())
            }
        }

        // Anyone in the room may send a typed turn — same "ask, owner acts"
        // shape as start/stop, not the "only for yourself" shape PTT needs:
        // there is no floor to misattribute, the host attributes the turn to
        // the authenticated sender exactly like it does for PTT's speaker.
        Body::SendText { .. } => Ok(()),
    }
}

/// Anti-flood: how many start/stop requests one sender may make per minute
/// before the owner ignores them. A peer must not be able to churn the host's
/// WebSocket session — or burn its token budget — by spamming the topic.
pub const MAX_REQUESTS_PER_SENDER_PER_MINUTE: u32 = 5;

/// A typed turn is capped the same length skylumi's own text-harness caps its
/// typed turns at — long enough for a real question, short enough that one
/// message can't dominate a turn or blow past Gemini's practical prompt size.
pub const MAX_USER_TEXT_CHARS: usize = 600;

/// Anti-flood for typed turns — a SEPARATE budget from start/stop and from
/// PTT. A typed message is naturally rate-limited by typing speed, but
/// without its own budget one impatient peer mashing "send" could still
/// burn the host's token budget as fast as PTT spam could.
pub const MAX_TEXT_SENDS_PER_SENDER_PER_MINUTE: u32 = 20;

/// Anti-flood for push-to-talk claims — a SEPARATE budget from start/stop.
///
/// `pttStart` is not free: it claims the floor, taps the claimant's LiveKit
/// audio track and publishes a `state` to the whole room. It shipped with no
/// limit at all, because the limiter sat inside the start/stop arm only (#661).
/// But it is a human key press, not a session churn, so it cannot share the
/// 5/minute budget above — that would cut a normal back-and-forth off after a
/// few sentences. `pttEnd` is deliberately NOT limited: dropping one wedges the
/// floor until the silence timeout.
pub const MAX_PTT_STARTS_PER_SENDER_PER_MINUTE: u32 = 30;

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> WindowKey {
        WindowKey {
            window_id: 42,
            owner_identity: "owner-alice".to_string(),
        }
    }

    fn msg(body: Body) -> Message {
        Message {
            v: VERSION,
            key: key(),
            body,
        }
    }

    #[test]
    fn anyone_may_request_start_or_stop() {
        for body in [Body::StartRequest, Body::StopRequest] {
            assert_eq!(authorize(&msg(body.clone()), "peer-bob"), Ok(()));
            assert_eq!(authorize(&msg(body), "owner-alice"), Ok(()));
        }
    }

    #[test]
    fn only_the_owner_may_author_state() {
        let state = Body::State {
            active: true,
            started_by: Some("peer-bob".into()),
            seconds_left: Some(120),
            active_speaker: None,
            error: None,
        };
        assert_eq!(authorize(&msg(state.clone()), "owner-alice"), Ok(()));
        // A peer claiming a session is running on someone else's window.
        assert_eq!(
            authorize(&msg(state), "peer-bob"),
            Err(Rejection::NotWindowOwner)
        );
    }

    #[test]
    fn only_the_owner_may_author_transcript() {
        let line = Body::Transcript {
            role: TranscriptRole::Assistant,
            text: "the deploy is green".into(),
            final_: true,
        };
        assert_eq!(authorize(&msg(line.clone()), "owner-alice"), Ok(()));
        // Otherwise a peer could put words in the assistant's mouth, and every
        // surface would render them as authoritative.
        assert_eq!(
            authorize(&msg(line), "peer-mallory"),
            Err(Rejection::NotWindowOwner)
        );
    }

    #[test]
    fn ptt_requires_an_authenticated_sender() {
        assert_eq!(authorize(&msg(Body::PttStart), "peer-bob"), Ok(()));
        assert_eq!(
            authorize(&msg(Body::PttStart), ""),
            Err(Rejection::NotSelf)
        );
    }

    #[test]
    fn anyone_may_send_text() {
        // Unlike PTT, there is nothing to misattribute -- the host attributes
        // the typed turn to the authenticated sender exactly like it does for
        // PTT's speaker, so any current participant may send one.
        let body = Body::SendText {
            text: "what does this button do?".into(),
        };
        assert_eq!(authorize(&msg(body.clone()), "peer-bob"), Ok(()));
        assert_eq!(authorize(&msg(body), "owner-alice"), Ok(()));
    }

    #[test]
    fn a_future_version_is_rejected_outright() {
        let mut m = msg(Body::StartRequest);
        m.v = VERSION + 1;
        assert_eq!(authorize(&m, "owner-alice"), Err(Rejection::UnsupportedVersion));
    }

    #[test]
    fn ai_track_names_are_recognizable_and_scoped_to_a_window() {
        let name = ai_track_name(77);
        assert!(is_ai_track(&name), "{name}");
        assert!(name.contains("77"));
        // Must not collide with the existing window/camera track namespaces —
        // every prefix-matching parser has to classify this distinctly.
        assert!(!is_ai_track("petal-window-77"));
        assert!(!is_ai_track("petal-camera-alice"));
    }

    #[test]
    fn state_round_trips_and_omits_absent_fields() {
        let m = msg(Body::State {
            active: false,
            started_by: None,
            seconds_left: None,
            active_speaker: None,
            error: Some(EndReason::Disabled),
        });
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"type\":\"state\""), "{json}");
        assert!(json.contains("\"error\":\"disabled\""), "{json}");
        assert!(json.contains("\"windowId\":42"), "{json}");
        assert!(json.contains("\"ownerIdentity\":\"owner-alice\""), "{json}");
        // Absent optionals must not appear as nulls; the web client parses this.
        assert!(!json.contains("startedBy"), "{json}");
        assert!(!json.contains("secondsLeft"), "{json}");

        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn transcript_round_trips() {
        let m = msg(Body::Transcript {
            role: TranscriptRole::User,
            text: "what does this error mean?".into(),
            final_: false,
        });
        let json = serde_json::to_string(&m).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(back, m);
    }

    /// The cross-language fixture. Rust and the web client both assert against
    /// this file, which is what stops the two implementations from drifting.
    const CONTRACTS: &str = include_str!("../../../../../contracts/petal-contracts.json");

    #[test]
    fn topic_and_track_prefix_match_the_pinned_contract() {
        let contracts: serde_json::Value = serde_json::from_str(CONTRACTS).unwrap();
        assert_eq!(
            contracts["topics"]["aiChat"].as_str(),
            Some(TOPIC),
            "topic drifted from contracts/petal-contracts.json"
        );
        for case in contracts["aiTracks"].as_array().unwrap() {
            let window_id = case["windowId"].as_u64().unwrap() as u32;
            let expected = case["trackName"].as_str().unwrap();
            assert_eq!(ai_track_name(window_id), expected);
            assert!(is_ai_track(expected));
        }
    }

    /// Round-tripping is NOT enough to prove wire compatibility: serde ignores
    /// unknown fields on the way in, so a message we serialize with the wrong
    /// key names still deserializes back into an equal value. This asserts the
    /// exact key SET we emit matches the exact key set the contract specifies —
    /// which is the check that would have caught the `started_by` /
    /// `seconds_left` / `final_` break the web client found.
    #[test]
    fn emitted_json_keys_match_the_contract_exactly() {
        let contracts: serde_json::Value = serde_json::from_str(CONTRACTS).unwrap();
        for case in contracts["aiChatMessages"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let expected = case["message"].as_object().unwrap();
            let parsed: Message = serde_json::from_value(case["message"].clone())
                .unwrap_or_else(|e| panic!("fixture '{name}' does not parse: {e}"));
            let emitted = serde_json::to_value(&parsed).unwrap();
            let emitted = emitted.as_object().unwrap();

            let mut expected_keys: Vec<&String> = expected.keys().collect();
            let mut emitted_keys: Vec<&String> = emitted.keys().collect();
            expected_keys.sort();
            emitted_keys.sort();
            assert_eq!(
                emitted_keys, expected_keys,
                "fixture '{name}': emitted JSON keys differ from the contract"
            );
            // And the values must survive, not merely the names.
            for (key, value) in expected {
                assert_eq!(
                    emitted.get(key),
                    Some(value),
                    "fixture '{name}': value for '{key}' differs"
                );
            }
        }
    }

    #[test]
    fn pinned_messages_deserialize_into_this_module() {
        let contracts: serde_json::Value = serde_json::from_str(CONTRACTS).unwrap();
        for case in contracts["aiChatMessages"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let parsed: Message = serde_json::from_value(case["message"].clone())
                .unwrap_or_else(|e| panic!("fixture '{name}' does not parse: {e}"));
            assert_eq!(parsed.v, VERSION, "fixture '{name}'");

            // The fixture records who may send each kind; assert the matrix
            // actually enforces it, so the documented contract and the code
            // cannot disagree.
            let owner = parsed.key.owner_identity.clone();
            match case["authorizedSenders"].as_str().unwrap() {
                "any-participant" => {
                    assert_eq!(authorize(&parsed, "someone-else"), Ok(()), "{name}");
                }
                "window-owner-only" => {
                    assert_eq!(authorize(&parsed, &owner), Ok(()), "{name}");
                    assert_eq!(
                        authorize(&parsed, "someone-else"),
                        Err(Rejection::NotWindowOwner),
                        "{name}"
                    );
                }
                "self-only" => {
                    assert_eq!(authorize(&parsed, "someone-else"), Ok(()), "{name}");
                    assert_eq!(authorize(&parsed, ""), Err(Rejection::NotSelf), "{name}");
                }
                other => panic!("unknown authorizedSenders '{other}' in fixture '{name}'"),
            }
        }
    }

    #[test]
    fn every_end_reason_is_pinned_in_the_contract() {
        // The web client renders copy from these tokens; adding a variant here
        // without adding it to the contract would silently break that surface.
        let contracts: serde_json::Value = serde_json::from_str(CONTRACTS).unwrap();
        let pinned: Vec<&str> = contracts["aiChatEndReasons"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
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
        assert_eq!(all.len(), pinned.len(), "reason count drifted");
        for reason in all {
            let token = serde_json::to_value(reason).unwrap();
            let token = token.as_str().unwrap();
            assert!(pinned.contains(&token), "{token} missing from contract");
        }
    }

    #[test]
    fn push_to_talk_has_its_own_bounded_budget() {
        // Both directions. "Larger than start/stop" alone would be satisfied by
        // an effectively unlimited value, and "bounded" alone by a value so
        // small it silences a real conversation.
        assert!(
            MAX_PTT_STARTS_PER_SENDER_PER_MINUTE > MAX_REQUESTS_PER_SENDER_PER_MINUTE,
            "a human key press cannot share the session-churn budget"
        );
        assert!(
            (10..=60).contains(&MAX_PTT_STARTS_PER_SENDER_PER_MINUTE),
            "{MAX_PTT_STARTS_PER_SENDER_PER_MINUTE} is not a bound worth having"
        );
    }

    #[test]
    fn heartbeat_staleness_window_is_bounded_and_sane() {
        // Long enough to ride out a hiccup, short enough that a crashed host
        // does not leave a phantom "AI active" badge for long.
        let worst_case = STATE_HEARTBEAT_SECONDS * STATE_MISSED_HEARTBEATS_BEFORE_STALE as u64;
        assert!((10..=30).contains(&worst_case), "{worst_case}s");
    }
}
