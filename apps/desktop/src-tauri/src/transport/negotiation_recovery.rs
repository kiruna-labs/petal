//! #169: the publisher transport must recover from an answer libwebrtc
//! rejects while a local offer is outstanding. These tests drive the REAL
//! vendored `PeerTransport` against two in-process peer connections with no
//! network (SDP exchange only, never ICE), reproducing the field failure:
//! a simulcast answer that prunes a rid on a sender that was stopped before
//! the answer arrived ("Cannot disable encodings on a stopped sender").
//!
//! Before the patch the transport stayed in `HaveLocalOffer` forever after
//! that error (every later `create_and_send_offer` returned Ok without
//! sending); with it, the offer is rolled back and a replacement offer is
//! sent, whose answer applies and leaves the transport `Stable`.

use std::time::Duration;

use livekit::rtc_engine::peer_transport::{PeerTransport, RemoteDescriptionOutcome, SignalTarget};
use livekit::webrtc::peer_connection::{PeerConnection, SignalingState};
use livekit::webrtc::peer_connection_factory::{
    ContinualGatheringPolicy, IceTransportsType, PeerConnectionFactory,
};
use livekit::webrtc::prelude::*;
use livekit::webrtc::rtp_parameters::RtpEncodingParameters;
use livekit::webrtc::rtp_transceiver::{RtpTransceiverDirection, RtpTransceiverInit};
use livekit::webrtc::session_description::{SdpType, SessionDescription};
use livekit::webrtc::MediaType;

const STEP: Duration = Duration::from_secs(10);

fn pair() -> (PeerConnectionFactory, PeerConnection, PeerConnection) {
    let factory = PeerConnectionFactory::default();
    let config = RtcConfiguration {
        ice_servers: vec![],
        continual_gathering_policy: ContinualGatheringPolicy::GatherOnce,
        ice_transport_type: IceTransportsType::All,
    };
    let publisher = factory
        .create_peer_connection(config.clone())
        .expect("publisher pc");
    let sfu = factory.create_peer_connection(config).expect("sfu pc");
    (factory, publisher, sfu)
}

fn simulcast_init() -> RtpTransceiverInit {
    let encoding = |rid: &str| RtpEncodingParameters {
        active: true,
        rid: rid.to_string(),
        ..Default::default()
    };
    RtpTransceiverInit {
        direction: RtpTransceiverDirection::SendOnly,
        stream_ids: vec!["petal".to_string()],
        send_encodings: vec![encoding("q"), encoding("h"), encoding("f")],
    }
}

/// The SFU's answer to a simulcast offer, the way LiveKit answers it: the
/// rids it will receive plus `a=simulcast:recv`, with one rid pruned. A bare
/// libwebrtc answerer emits no simulcast lines at all, so the test appends
/// them the way the server does. Pure string work so the SDP is exact.
fn with_simulcast_recv(answer: &str, rids: &[&str]) -> String {
    let mut lines: Vec<String> = answer
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    for rid in rids {
        lines.push(format!("a=rid:{rid} recv"));
    }
    lines.push(format!("a=simulcast:recv {}", rids.join(";")));
    lines.join("\r\n") + "\r\n"
}

async fn sfu_answer(sfu: &PeerConnection, offer: SessionDescription) -> String {
    sfu.set_remote_description(offer).await.expect("sfu takes the offer");
    let answer = sfu
        .create_answer(AnswerOptions::default())
        .await
        .expect("sfu answers");
    let text = answer.to_string();
    sfu.set_local_description(answer).await.expect("sfu applies its answer");
    text
}

fn transport_with_offer_channel(
    publisher: &PeerConnection,
) -> (PeerTransport, tokio::sync::mpsc::UnboundedReceiver<SessionDescription>) {
    let transport = PeerTransport::new(publisher.clone(), SignalTarget::Publisher, false);
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    transport.on_offer(Some(Box::new(move |offer| {
        let _ = tx.send(offer);
    })));
    (transport, rx)
}

/// The field shape end to end: a pruned-simulcast answer on a stopped sender
/// is rejected, the transport rolls back and re-offers, the replacement
/// answer applies, and the publisher ends `Stable` -- not wedged in
/// `HaveLocalOffer`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_answer_on_stopped_sender_is_rolled_back_and_renegotiated() {
    let (_factory, publisher, sfu) = pair();
    let (transport, mut offers) = transport_with_offer_channel(&publisher);
    let transceiver = publisher
        .add_transceiver_for_media(MediaType::Video, simulcast_init())
        .expect("simulcast transceiver");

    transport
        .create_and_send_offer(OfferOptions::default())
        .await
        .expect("first offer");
    let offer1 = tokio::time::timeout(STEP, offers.recv())
        .await
        .expect("offer within the step budget")
        .expect("offer");
    assert_eq!(publisher.signaling_state(), SignalingState::HaveLocalOffer);
    assert!(
        offer1.to_string().contains("a=simulcast:send q;h;f"),
        "the publisher must offer three simulcast rids"
    );
    let answer1 = sfu_answer(&sfu, offer1).await;
    // The SFU keeps q and f and prunes h -- dynacast's shape in the field.
    let pruned = SessionDescription::parse(&with_simulcast_recv(&answer1, &["q", "f"]), SdpType::Answer)
        .expect("pruned simulcast answer parses");

    // The pre-#183 trigger: the sender is stopped before its answer lands.
    transceiver.stop().expect("stop the sender");

    // Reproduce the raw libwebrtc rejection first, so the recovery below is
    // known to be recovering from the real failure and not from nothing.
    let raw = publisher.set_remote_description(pruned.clone()).await;
    let raw_error = raw.expect_err("a pruned answer on a stopped sender must be rejected");
    assert!(
        raw_error.message.contains("stopped sender"),
        "expected the field failure, got: {raw_error:?}"
    );
    assert_eq!(
        publisher.signaling_state(),
        SignalingState::HaveLocalOffer,
        "the rejection leaves the offer outstanding"
    );

    let outcome = tokio::time::timeout(STEP, transport.set_remote_description(pruned))
        .await
        .expect("recovery must not hang")
        .expect("recovery must not surface the rejection");
    assert_eq!(outcome, RemoteDescriptionOutcome::RolledBackAndRenegotiating);
    let offer2 = tokio::time::timeout(STEP, offers.recv())
        .await
        .expect("replacement offer within the step budget")
        .expect("replacement offer");
    assert_eq!(
        publisher.signaling_state(),
        SignalingState::HaveLocalOffer,
        "the replacement offer is outstanding, not the rejected one"
    );

    let answer2 = sfu_answer(&sfu, offer2).await;
    let applied = tokio::time::timeout(
        STEP,
        transport.set_remote_description(
            SessionDescription::parse(&answer2, SdpType::Answer).expect("answer parses"),
        ),
    )
    .await
    .expect("applying the replacement answer must not hang")
    .expect("the replacement answer applies");
    assert_eq!(applied, RemoteDescriptionOutcome::Applied);
    assert_eq!(
        publisher.signaling_state(),
        SignalingState::Stable,
        "the publisher is negotiated again, not wedged"
    );

    // And the transport can still offer: the wedge symptom was every later
    // create_and_send_offer returning Ok without ever sending.
    transport
        .create_and_send_offer(OfferOptions::default())
        .await
        .expect("a later offer");
    tokio::time::timeout(STEP, offers.recv())
        .await
        .expect("a later offer is actually sent")
        .expect("offer");
}

/// The lock hazard: `renegotiate` set while an offer was outstanding used to
/// be consumed by `set_remote_description` calling `create_and_send_offer`
/// with the transport mutex still held. The call must return, and the queued
/// re-offer must be sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_renegotiation_after_an_applied_answer_does_not_deadlock() {
    let (_factory, publisher, sfu) = pair();
    let (transport, mut offers) = transport_with_offer_channel(&publisher);
    publisher
        .add_transceiver_for_media(MediaType::Video, simulcast_init())
        .expect("transceiver");
    transport
        .create_and_send_offer(OfferOptions::default())
        .await
        .expect("first offer");
    let offer1 = offers.recv().await.expect("offer");
    // A second offer while the first is outstanding queues `renegotiate`.
    transport
        .create_and_send_offer(OfferOptions::default())
        .await
        .expect("queued offer returns Ok");
    assert!(
        offers.try_recv().is_err(),
        "nothing is sent while the first offer is outstanding"
    );

    let answer1 = sfu_answer(&sfu, offer1).await;
    let outcome = tokio::time::timeout(
        STEP,
        transport.set_remote_description(
            SessionDescription::parse(&answer1, SdpType::Answer).expect("answer parses"),
        ),
    )
    .await
    .expect("must not deadlock on the queued re-offer")
    .expect("answer applies");
    assert_eq!(outcome, RemoteDescriptionOutcome::Applied);
    tokio::time::timeout(STEP, offers.recv())
        .await
        .expect("the queued re-offer is sent after the answer applies")
        .expect("offer");
    assert_eq!(publisher.signaling_state(), SignalingState::HaveLocalOffer);
}
