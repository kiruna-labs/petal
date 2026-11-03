//! Room-level live-stream state on the wire, pinned by
//! contracts/petal-contracts.json (`streamLiveAttribute`,
//! `streamLiveDerivation`, `streamOptOutAttribute`):
//!
//! * `petal.live`, the sharer's own participant attribute. The room is LIVE
//!   when anyone has it.
//! * `petal.stream-optout`, set to `"1"` by a participant who keeps their
//!   audio off live streams.

use serde::{Deserialize, Serialize};

pub const STREAM_LIVE_ATTRIBUTE: &str = "petal.live";
pub const STREAM_OPTOUT_ATTRIBUTE: &str = "petal.stream-optout";
/// The only value that opts a participant out.
pub const STREAM_OPTOUT_VALUE: &str = "1";

/// Whether a `petal.stream-optout` value keeps that participant's audio off
/// live streams. Absent or anything but `"1"` means included.
pub fn is_opted_out(value: Option<&str>) -> bool {
    value == Some(STREAM_OPTOUT_VALUE)
}

/// The attribute value for this computer's own setting; an empty value
/// deletes the key.
pub fn opt_out_value(keep_off: bool) -> &'static str {
    if keep_off {
        STREAM_OPTOUT_VALUE
    } else {
        ""
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamShareBinding {
    pub owner_identity: String,
    pub window_id: u32,
}

/// The attribute's `mode`: always "client" (encoded on this computer);
/// anything else is rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamMode {
    Client,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClientLiveState {
    Live,
    Paused,
}

/// The `petal.live` attribute value. Field order is the wire order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientLiveAttribute {
    pub mode: StreamMode,
    pub started_at: u64,
    pub window_id: u32,
    pub state: ClientLiveState,
}

/// The `petal.live` attribute; `None` when absent or malformed.
pub fn parse_client_live_attribute(value: &str) -> Option<ClientLiveAttribute> {
    serde_json::from_str::<ClientLiveAttribute>(value).ok()
}

pub fn encode_client_live_attribute(attr: &ClientLiveAttribute) -> String {
    serde_json::to_string(attr).unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveStreamState {
    pub share: StreamShareBinding,
    pub started_at: u64,
    pub paused: bool,
}

/// Every live stream in the room from `(identity, petal.live)` pairs,
/// earliest first by `(startedAt, identity)`. One stream per room is the
/// rule; two only coexist for the moment it takes the later one to see the
/// earlier and stop ([`loses_arbitration`]).
pub fn derive_live_state<'a>(
    attributes: impl IntoIterator<Item = (&'a str, Option<&'a str>)>,
) -> Vec<LiveStreamState> {
    let mut streams: Vec<LiveStreamState> = attributes
        .into_iter()
        .filter_map(|(identity, value)| {
            let attr = parse_client_live_attribute(value?)?;
            Some(LiveStreamState {
                share: StreamShareBinding { owner_identity: identity.to_owned(), window_id: attr.window_id },
                started_at: attr.started_at,
                paused: attr.state == ClientLiveState::Paused,
            })
        })
        .collect();
    streams.sort_by(|a, b| {
        (a.started_at, a.share.owner_identity.as_str()).cmp(&(b.started_at, b.share.owner_identity.as_str()))
    });
    streams
}

/// Whether our own stream (`identity`, started at `started_at`) must give
/// way: another participant's stream sorts before it. Ties on `startedAt`
/// break on identity, so exactly one of two simultaneous streams survives.
pub fn loses_arbitration(identity: &str, started_at: u64, streams: &[LiveStreamState]) -> bool {
    streams.iter().any(|stream| {
        stream.share.owner_identity != identity
            && (stream.started_at, stream.share.owner_identity.as_str()) < (started_at, identity)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct AttributeFixture {
        key: String,
        value: String,
        parsed: ClientLiveAttribute,
        rejected: Vec<String>,
    }

    #[derive(Deserialize)]
    struct IdentityValue {
        identity: String,
        value: Option<String>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct StreamFixture {
        owner_identity: String,
        window_id: u32,
        paused: bool,
        started_at: u64,
    }

    #[derive(Deserialize)]
    struct DerivationFixture {
        attributes: Vec<IdentityValue>,
        streams: Vec<StreamFixture>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct OptOutFixture {
        key: String,
        opted_out: String,
        included: Vec<String>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Contracts {
        stream_live_attribute: AttributeFixture,
        stream_live_derivation: DerivationFixture,
        stream_opt_out_attribute: OptOutFixture,
    }

    fn contracts() -> Contracts {
        serde_json::from_str(include_str!("../../../../../contracts/petal-contracts.json")).unwrap()
    }

    #[test]
    fn parses_and_encodes_the_contract_attribute() {
        let fixture = contracts().stream_live_attribute;
        assert_eq!(fixture.key, STREAM_LIVE_ATTRIBUTE);
        assert_eq!(parse_client_live_attribute(&fixture.value), Some(fixture.parsed));
        assert_eq!(encode_client_live_attribute(&fixture.parsed), fixture.value);
        for rejected in fixture.rejected {
            assert_eq!(parse_client_live_attribute(&rejected), None, "{rejected}");
        }
    }

    #[test]
    fn any_attribute_makes_the_room_live() {
        assert!(derive_live_state([("a", None)]).is_empty());
        let streams = derive_live_state([
            ("a", None),
            ("b", Some(r#"{"mode":"client","startedAt":5,"windowId":9,"state":"paused"}"#)),
        ]);
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].share, StreamShareBinding { owner_identity: "b".into(), window_id: 9 });
        assert!(streams[0].paused);
    }

    #[test]
    fn derives_every_stream_earliest_first_from_the_contract() {
        let fixture = contracts().stream_live_derivation;
        let streams = derive_live_state(
            fixture.attributes.iter().map(|entry| (entry.identity.as_str(), entry.value.as_deref())),
        );
        let got: Vec<(String, u32, bool, u64)> = streams
            .iter()
            .map(|s| (s.share.owner_identity.clone(), s.share.window_id, s.paused, s.started_at))
            .collect();
        let want: Vec<(String, u32, bool, u64)> = fixture
            .streams
            .into_iter()
            .map(|s| (s.owner_identity, s.window_id, s.paused, s.started_at))
            .collect();
        assert_eq!(got, want);
    }

    fn live(started_at: u64) -> String {
        format!(r#"{{"mode":"client","startedAt":{started_at},"windowId":1,"state":"live"}}"#)
    }

    #[test]
    fn the_later_of_two_streams_gives_way() {
        let (ten, twenty) = (live(10), live(20));
        let streams = derive_live_state([("bob", Some(ten.as_str())), ("alice", Some(twenty.as_str()))]);
        assert!(loses_arbitration("alice", 20, &streams));
        assert!(!loses_arbitration("bob", 10, &streams));
        // The same instant: identity breaks the tie, so exactly one survives.
        let tied = derive_live_state([("bob", Some(ten.as_str())), ("alice", Some(ten.as_str()))]);
        assert!(!loses_arbitration("alice", 10, &tied));
        assert!(loses_arbitration("bob", 10, &tied));
        // Alone (our own attribute set or not yet): never.
        let alone = derive_live_state([("alice", Some(twenty.as_str()))]);
        assert!(!loses_arbitration("alice", 20, &alone));
        assert!(!loses_arbitration("alice", 20, &[]));
    }

    #[test]
    fn only_the_contract_value_opts_out() {
        let fixture = contracts().stream_opt_out_attribute;
        assert_eq!(fixture.key, STREAM_OPTOUT_ATTRIBUTE);
        assert_eq!(fixture.opted_out, STREAM_OPTOUT_VALUE);
        assert!(is_opted_out(Some(&fixture.opted_out)));
        assert!(!is_opted_out(None));
        for included in fixture.included {
            assert!(!is_opted_out(Some(&included)), "{included:?}");
        }
        assert_eq!(opt_out_value(true), "1");
        assert_eq!(opt_out_value(false), "");
    }
}
