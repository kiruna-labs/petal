//! #298 publication-reconciliation probe: measure, against a real SFU,
//! whether a receiver whose state has DIVERGED from the SFU's actual
//! publications can detect that and reconverge -- and prove it could not
//! before.
//!
//! Two peers, one process, one local LiveKit server. Peer A publishes a
//! `petal-window-<id>` share; peer B mirrors `subscriber.rs`'s real receiver
//! bookkeeping (the `WINDOW_PUBLICATIONS` sid-keyed registry, written on
//! `TrackSubscribed` and cleared under `should_remove_window`) and counts
//! decoded frames per sid.
//!
//! Four divergences are induced for real -- not simulated in a unit test --
//! and each is first shown to PERSIST with reconciliation off (`--no-reconcile`,
//! the positive control that proves the harness can see the failure), then
//! shown to reconverge with it on:
//!
//!   1. **lost TrackSubscribed** -- B keeps the subscription but loses its
//!      receiver state, the reported #298 symptom: the share is live on the
//!      SFU and nothing local ever notices it is gone. Run FIRST, because it
//!      needs a publication the SDK still holds a track for.
//!   2. **subscription churn**  -- B's subscription demand is dropped.
//!   3. **publication replacement** -- A republishes the same window id under
//!      a new sid; B's teardown guard is left pointed at the dead one.
//!   4. **orphan** -- A unpublishes while B drops the event; B is left
//!      asserting a share the SFU does not hold.
//!
//! The crux measurement is what recovery levers actually exist. An earlier
//! draft of the contract assumed `set_subscribed(false)` then
//! `set_subscribed(true)` would re-emit `TrackSubscribed`; this probe measured
//! that it does NOT (frames resumed 244 -> 423 with zero re-emitted events and
//! `is_subscribed()` still false), because `is_subscribed()` is
//! `track().is_some()` and only a NEW peer-connection media track ever restores
//! that handle. The contract was redesigned around the measurement, and phase 1
//! now asserts the resulting rule: a track the SDK already holds is reported as
//! terminal rather than "recovered" without evidence.
//!
//! Usage (needs a LiveKit server; a local dev one is fine):
//!
//! ```sh
//! LIVEKIT_URL=ws://localhost:7897 \
//! LIVEKIT_API_KEY=devkey \
//! LIVEKIT_API_SECRET=secretsecretsecretsecretsecretsecret \
//!   cargo run --example publication_reconcile_probe -- [--no-reconcile]
//! ```
//!
//! Experiment loop for #298 work -- not cockpit apparatus and not a runtime
//! diagnostic subsystem (the project history §2.1).

use desktop_lib::transport::reconcile::{
    discover_window_publications, plan_recovery_steps, recovery_step, Divergence, RecoveryLedger,
    RecoveryStep, TrackedWindow, ORPHANED_GRACE,
};
use futures::StreamExt;
use livekit::options::{TrackPublishOptions, VideoCodec, VideoEncoding};
use livekit::prelude::*;
use livekit::track::{LocalTrack, LocalVideoTrack, RemoteTrack};
use livekit::webrtc::video_frame::{I420Buffer, VideoFrame, VideoRotation};
use livekit::webrtc::video_source::native::NativeVideoSource;
use livekit::webrtc::video_source::{RtcVideoSource, VideoResolution};
use livekit::webrtc::video_stream::native::NativeVideoStream;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const WINDOW_ID: u32 = 298;
const SOURCE_WIDTH: u32 = 1280;
const SOURCE_HEIGHT: u32 = 720;
const MIN_FRAME_EVIDENCE: u64 = 6;
const MIN_CHURN_EVENTS: u64 = 1;

/// How long each phase waits for the SFU/SDK to settle before sampling.
const SETTLE: Duration = Duration::from_secs(3);

fn token_for(identity: &str, room: &str, publish: bool, subscribe: bool) -> String {
    desktop_lib::transport::mint_access_token(identity, room, publish, subscribe).unwrap_or_else(
        |e| {
            eprintln!("failed to mint access token: {e}");
            std::process::exit(1);
        },
    )
}

fn publish_options() -> TrackPublishOptions {
    TrackPublishOptions {
        source: TrackSource::Screenshare,
        video_codec: VideoCodec::H264,
        simulcast: false,
        video_encoding: Some(VideoEncoding {
            max_bitrate: 2_000_000,
            max_framerate: 30.0,
        }),
        ..Default::default()
    }
}

/// Publish one `petal-window-<id>` track and drive real changing content into
/// it. A static frame lets the encoder coast, which would make "frames
/// resumed" unmeasurable.
async fn publish_share(room: &Room, stop: Arc<AtomicBool>) -> TrackSid {
    let source = NativeVideoSource::new(
        VideoResolution {
            width: SOURCE_WIDTH,
            height: SOURCE_HEIGHT,
        },
        true,
    );
    let track = LocalVideoTrack::create_video_track(
        &format!("petal-window-{WINDOW_ID}"),
        RtcVideoSource::Native(source.clone()),
    );
    let publication = room
        .local_participant()
        .publish_track(LocalTrack::Video(track), publish_options())
        .await
        .unwrap_or_else(|e| {
            eprintln!("publish failed: {e}");
            std::process::exit(1);
        });

    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(33));
        let mut n: u32 = 0;
        while !stop.load(Ordering::Relaxed) {
            tick.tick().await;
            let mut buf = I420Buffer::new(SOURCE_WIDTH, SOURCE_HEIGHT);
            let band = (n * 11) % SOURCE_HEIGHT;
            {
                let (y, u, v) = buf.data_mut();
                for row in 0..SOURCE_HEIGHT as usize {
                    let luma = if row.abs_diff(band as usize) < 60 { 235 } else { 16 };
                    let start = row * SOURCE_WIDTH as usize;
                    y[start..start + SOURCE_WIDTH as usize].fill(luma);
                }
                u.fill(128);
                v.fill(128);
            }
            source.capture_frame(&VideoFrame {
                rotation: VideoRotation::VideoRotation0,
                timestamp_us: 0,
                frame_metadata: None,
                buffer: &buf,
            });
            n = n.wrapping_add(1);
        }
    });

    publication.sid()
}

/// Peer B's receiver state, shaped exactly like `subscriber.rs`'s real
/// `WINDOW_PUBLICATIONS` registry: an entry exists while a decode loop does.
#[derive(Default)]
struct Receiver {
    /// (owner, window_id) -> track sid currently being decoded.
    registry: Mutex<HashMap<(String, u32), String>>,
    /// Decoded frames, per track sid.
    frames: Mutex<HashMap<String, Arc<AtomicU64>>>,
    /// Count of `TrackSubscribed` events seen, per sid -- how the crux
    /// measurement (does a resubscribe re-emit?) is read.
    subscribes: Mutex<HashMap<String, u32>>,
    /// Real TrackUnsubscribed events, the expected evidence of phase 2 churn.
    unsubscribes: AtomicU64,
    /// When set, B ignores teardown events, reproducing a lost/dropped event.
    swallow_teardown: AtomicBool,
    /// When set, B ignores `TrackSubscribed`, reproducing the callback loss
    /// that leaves the teardown guard pinned to a superseded sid.
    swallow_subscribe: AtomicBool,
}

impl Receiver {
    fn tracked(&self) -> Vec<TrackedWindow> {
        self.registry
            .lock()
            .unwrap()
            .iter()
            .map(|((owner, window_id), sid)| TrackedWindow {
                owner_identity: owner.clone(),
                window_id: *window_id,
                sid: sid.clone(),
            })
            .collect()
    }

    fn frames_for(&self, sid: &str) -> u64 {
        self.frames
            .lock()
            .unwrap()
            .get(sid)
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    fn total_frames(&self) -> u64 {
        self.frames
            .lock()
            .unwrap()
            .values()
            .map(|c| c.load(Ordering::Relaxed))
            .sum()
    }

    fn subscribe_count(&self, sid: &str) -> u32 {
        self.subscribes
            .lock()
            .unwrap()
            .get(sid)
            .copied()
            .unwrap_or(0)
    }

    fn unsubscribe_count(&self) -> u64 {
        self.unsubscribes.load(Ordering::Relaxed)
    }
}

struct Verdict {
    name: &'static str,
    pass: bool,
    detail: String,
}

fn main() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime.block_on(run());
}

async fn run() {
    let _ = dotenvy::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/../.env"));
    let args: Vec<String> = std::env::args().skip(1).collect();
    // The positive control: induce every divergence but never apply the
    // reconciliation contract's recovery, and watch each one persist.
    let no_reconcile = args.iter().any(|a| a == "--no-reconcile");

    let url = desktop_lib::transport::token::livekit_url().unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });
    let room_name = format!(
        "petal-298-probe-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    );

    println!("=== #298 publication-reconciliation probe ===");
    println!("  room        : {room_name}");
    println!("  window id   : {WINDOW_ID}");
    println!(
        "  recovery    : {}\n",
        if no_reconcile {
            "OFF  <-- POSITIVE CONTROL: divergence induced, contract not applied"
        } else {
            "ON   (transport::reconcile decision table applied)"
        }
    );

    let stop = Arc::new(AtomicBool::new(false));
    let receiver = Arc::new(Receiver::default());

    // ---- peer B: subscriber ------------------------------------------------
    let (sub_room, mut sub_events) = Room::connect(
        &url,
        &token_for("petal-298-sub", &room_name, false, true),
        RoomOptions::default(),
    )
    .await
    .expect("subscriber connect");
    let sub_room = Arc::new(sub_room);

    {
        let receiver = receiver.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            while let Some(event) = sub_events.recv().await {
                if matches!(&event, RoomEvent::TrackUnsubscribed { .. }) {
                    receiver.unsubscribes.fetch_add(1, Ordering::Relaxed);
                }
                match event {
                    // Mirrors subscriber.rs's real TrackSubscribed arm: register
                    // the sid, then spawn a FRESH decode loop per event (which
                    // is what makes same-window-id republish work, and why
                    // reconciliation must never become a second discovery path).
                    RoomEvent::TrackSubscribed {
                        track,
                        publication,
                        participant,
                    } => {
                        let RemoteTrack::Video(video) = track else {
                            continue;
                        };
                        let name = video.name();
                        let Some(window_id) = name
                            .strip_prefix("petal-window-")
                            .and_then(|id| id.parse::<u32>().ok())
                        else {
                            continue;
                        };
                        let sid = publication.sid().to_string();
                        let owner = participant.identity().to_string();
                        if receiver.swallow_subscribe.load(Ordering::Relaxed) {
                            println!(
                                "  [B] subscribe SWALLOWED  window={window_id} sid={sid} (inducing divergence)"
                            );
                            continue;
                        }
                        println!("  [B] TrackSubscribed      window={window_id} sid={sid}");
                        receiver
                            .registry
                            .lock()
                            .unwrap()
                            .insert((owner, window_id), sid.clone());
                        *receiver
                            .subscribes
                            .lock()
                            .unwrap()
                            .entry(sid.clone())
                            .or_insert(0) += 1;
                        let counter = receiver
                            .frames
                            .lock()
                            .unwrap()
                            .entry(sid.clone())
                            .or_insert_with(|| Arc::new(AtomicU64::new(0)))
                            .clone();
                        let stop = stop.clone();
                        tokio::spawn(async move {
                            let mut stream = NativeVideoStream::new(video.rtc_track());
                            while let Some(_frame) = stream.next().await {
                                if stop.load(Ordering::Relaxed) {
                                    break;
                                }
                                counter.fetch_add(1, Ordering::Relaxed);
                            }
                        });
                    }
                    RoomEvent::TrackUnsubscribed {
                        publication,
                        participant,
                        ..
                    }
                    | RoomEvent::TrackUnpublished {
                        publication,
                        participant,
                    } => {
                        let name = publication.name();
                        let Some(window_id) = name
                            .strip_prefix("petal-window-")
                            .and_then(|id| id.parse::<u32>().ok())
                        else {
                            continue;
                        };
                        let sid = publication.sid().to_string();
                        if receiver.swallow_teardown.load(Ordering::Relaxed) {
                            println!(
                                "  [B] teardown SWALLOWED   window={window_id} sid={sid} (inducing divergence)"
                            );
                            continue;
                        }
                        let owner = participant.identity().to_string();
                        let key = (owner, window_id);
                        let mut registry = receiver.registry.lock().unwrap();
                        // subscriber.rs's real should_remove_window guard.
                        if registry.get(&key).map(String::as_str) == Some(sid.as_str()) {
                            registry.remove(&key);
                            println!("  [B] teardown             window={window_id} sid={sid}");
                        } else {
                            println!(
                                "  [B] teardown ignored     window={window_id} sid={sid} (superseded)"
                            );
                        }
                    }
                    _ => {}
                }
            }
        });
    }

    // ---- peer A: publisher -------------------------------------------------
    let (pub_room, mut pub_events) = Room::connect(
        &url,
        &token_for("petal-298-pub", &room_name, true, false),
        RoomOptions::default(),
    )
    .await
    .expect("publisher connect");
    tokio::spawn(async move { while pub_events.recv().await.is_some() {} });

    let first_sid = publish_share(&pub_room, stop.clone()).await.to_string();
    println!("  [A] published            sid={first_sid}");
    tokio::time::sleep(SETTLE).await;

    let mut verdicts: Vec<Verdict> = Vec::new();

    // ---- baseline ----------------------------------------------------------
    let baseline_frames = receiver.frames_for(&first_sid);
    let findings = desktop_lib::transport::reconcile::reconcile(
        &discover_window_publications(&sub_room),
        &receiver.tracked(),
    );
    verdicts.push(Verdict {
        name: "baseline: healthy share reconciles clean and frames flow",
        pass: findings.is_empty() && baseline_frames > 0,
        detail: format!("frames={baseline_frames} findings={findings:?}"),
    });
    println!("\n--- baseline: {baseline_frames} frames, findings={findings:?}\n");

    // ---- 1. lost TrackSubscribed (the reported #298 symptom) --------------
    // Deliberately first: it needs a publication whose track the SDK still
    // holds, which phase 2's churn would destroy (module doc hazard 1b).
    println!("--- phase 1: lost TrackSubscribed (SDK holds the track, nothing receiving)");
    let live_sid = window_publication(&sub_room)
        .map(|p| p.sid().to_string())
        .unwrap_or_default();
    let subscribes_before = receiver.subscribe_count(&live_sid);
    let sdk_holds_track = window_publication(&sub_room)
        .map(|p| p.is_subscribed())
        .unwrap_or(false);
    // Drop B's receiver state WITHOUT unsubscribing: the share stays live on
    // the SFU, and nothing local can ever notice it is no longer arriving.
    receiver.registry.lock().unwrap().clear();
    tokio::time::sleep(Duration::from_secs(1)).await;

    let findings = desktop_lib::transport::reconcile::reconcile(
        &discover_window_publications(&sub_room),
        &receiver.tracked(),
    );
    let lost_named = findings
        .iter()
        .any(|f| f.divergence == Divergence::NotReceiving);
    let not_receiving: Vec<_> = findings
        .iter()
        .filter(|f| f.divergence == Divergence::NotReceiving)
        .collect();
    let not_receiving_steps: Vec<_> = not_receiving
        .iter()
        .map(|f| recovery_step(&f.divergence, None, None))
        .collect();
    println!("  sdk_holds_track={sdk_holds_track} findings={findings:?}");
    verdicts.push(Verdict {
        name: "1. a live publication with no receiver state is named NotReceiving",
        pass: sdk_holds_track && lost_named,
        detail: format!("sdk_holds_track={sdk_holds_track} findings={findings:?}"),
    });
    verdicts.push(Verdict {
        name: "1. CRUX: an unrecoverable divergence reports truth, never a fake recovery",
        pass: !not_receiving.is_empty()
            && not_receiving_steps
                .iter()
                .all(|step| *step == RecoveryStep::ReportTruth),
        detail: if not_receiving.is_empty() {
            // Zero-evidence control (#622): `findings=[]` now reports this
            // instead of a vacuous PASS from `.all()`.
            "INSUFFICIENT DATA: no NotReceiving finding was evaluated".to_string()
        } else {
            format!(
                "evaluated {} NotReceiving finding(s): recovery steps={not_receiving_steps:?}",
                not_receiving.len()
            )
        },
    });
    // Restore B's state for the next phase (probe bookkeeping, not recovery).
    if !live_sid.is_empty() {
        receiver
            .registry
            .lock()
            .unwrap()
            .insert(("petal-298-pub".to_string(), WINDOW_ID), live_sid.clone());
    }
    let _ = subscribes_before;

    // ---- 2. subscription churn --------------------------------------------
    println!("\n--- phase 2: subscription churn (demand dropped)");
    let unsubscribes_before_churn = receiver.unsubscribe_count();
    {
        let publication = window_publication(&sub_room).expect("publication present");
        publication.set_subscribed(false);
    }
    tokio::time::sleep(SETTLE).await;
    let before = receiver.total_frames();
    tokio::time::sleep(Duration::from_secs(1)).await;
    let stalled = receiver.total_frames() == before;

    let findings = desktop_lib::transport::reconcile::reconcile(
        &discover_window_publications(&sub_room),
        &receiver.tracked(),
    );
    let churn_named = findings
        .iter()
        .any(|f| f.divergence == Divergence::NotSubscribed);
    let churn_events = receiver.unsubscribe_count() - unsubscribes_before_churn;
    println!("  frames stalled={stalled} findings={findings:?}");
    verdicts.push(Verdict {
        name: "2. subscription churn is detected and named NotSubscribed",
        pass: stalled && churn_named,
        detail: format!("stalled={stalled} findings={findings:?}"),
    });

    if !no_reconcile {
        apply_recovery(&sub_room, &findings);
        tokio::time::sleep(SETTLE).await;
    } else {
        tokio::time::sleep(SETTLE).await;
    }
    let after = receiver.total_frames();
    let recovered = after > before + 5;
    let churn_evidence = baseline_frames >= MIN_FRAME_EVIDENCE
        && before >= MIN_FRAME_EVIDENCE
        && churn_named
        && churn_events >= MIN_CHURN_EVENTS;
    verdicts.push(Verdict {
        name: if no_reconcile {
            "2. CONTROL: without recovery the churn never resolves"
        } else {
            "2. one recovery attempt restores the stream"
        },
        // Both arms must first prove a live stream existed and that the
        // intended NotSubscribed divergence was observed. A dead room is not
        // a passing no-reconcile control.
        // Zero-evidence control (#622): frames=0/events=0 emits
        // `[FAIL] ... CONTROL ... INSUFFICIENT DATA`, never PASS.
        pass: churn_evidence && if no_reconcile { !recovered } else { recovered },
        detail: if churn_evidence {
            format!(
                "evidence: baseline_frames={baseline_frames}, before={before}, \
                 NotSubscribed=true, churn_events={churn_events}; frames {before} -> {after}"
            )
        } else {
            format!(
                "INSUFFICIENT DATA: baseline_frames={baseline_frames} (need >= {MIN_FRAME_EVIDENCE}), \
                 before={before} (need >= {MIN_FRAME_EVIDENCE}), NotSubscribed={churn_named}; \
                 churn_events={churn_events} (need >= {MIN_CHURN_EVENTS}); frames {before} -> {after}"
            )
        },
    });
    println!("  after: frames {before} -> {after}\n");

    // ---- 3. publication replacement ---------------------------------------
    println!("--- phase 3: publication replacement (same window id, new sid)");
    let old_sid = window_publication(&sub_room)
        .map(|p| p.sid().to_string())
        .unwrap_or_default();
    // Pin B's guard to the OLD sid and swallow BOTH the replacement's
    // subscribe and the old track's teardown, so the registry is left
    // superseded exactly as a dropped pair of callbacks would leave it.
    receiver.registry.lock().unwrap().insert(
        ("petal-298-pub".to_string(), WINDOW_ID),
        old_sid.clone(),
    );
    receiver.swallow_teardown.store(true, Ordering::Relaxed);
    receiver.swallow_subscribe.store(true, Ordering::Relaxed);
    let replacement_sid = publish_share(&pub_room, stop.clone()).await.to_string();
    println!("  [A] republished          old={old_sid} new={replacement_sid}");
    for publication in pub_room.local_participant().track_publications().values() {
        if publication.sid().to_string() == old_sid {
            let _ = pub_room
                .local_participant()
                .unpublish_track(&publication.sid())
                .await;
        }
    }
    tokio::time::sleep(SETTLE).await;
    receiver.swallow_teardown.store(false, Ordering::Relaxed);
    receiver.swallow_subscribe.store(false, Ordering::Relaxed);

    let findings = desktop_lib::transport::reconcile::reconcile(
        &discover_window_publications(&sub_room),
        &receiver.tracked(),
    );
    let replacement_named = findings.iter().any(|f| {
        matches!(&f.divergence, Divergence::Replaced { to, .. } if to == &replacement_sid)
    });
    let replacements: Vec<_> = findings
        .iter()
        .filter(|f| matches!(f.divergence, Divergence::Replaced { .. }))
        .collect();
    let replacement_steps: Vec<_> = replacements
        .iter()
        .map(|f| recovery_step(&f.divergence, None, None))
        .collect();
    println!("  findings={findings:?}");
    verdicts.push(Verdict {
        name: "3. a superseded sid is named Replaced and carries the authoritative one",
        pass: replacement_named,
        detail: format!("old={old_sid} new={replacement_sid} findings={findings:?}"),
    });
    verdicts.push(Verdict {
        name: "3. a replacement is Adopt, never a resubscribe (no second decode loop)",
        pass: !replacements.is_empty()
            && replacement_steps
                .iter()
                .all(|step| *step == RecoveryStep::Adopt),
        detail: if replacements.is_empty() {
            "INSUFFICIENT DATA: no Replaced finding was evaluated".to_string()
        } else {
            format!(
                "evaluated {} Replaced finding(s): recovery steps={replacement_steps:?}",
                replacements.len()
            )
        },
    });
    // Adopt for real, so phase 4 starts from a current guard.
    if !no_reconcile {
        for finding in &findings {
            if let (Divergence::Replaced { to, .. }, Some(_)) =
                (&finding.divergence, finding.sid.as_ref())
            {
                receiver.registry.lock().unwrap().insert(
                    (finding.owner_identity.clone(), finding.window_id),
                    to.clone(),
                );
            }
        }
    }

    // ---- 4. orphan ---------------------------------------------------------
    println!("\n--- phase 4: orphan (publication gone, teardown event dropped)");
    receiver.swallow_teardown.store(true, Ordering::Relaxed);
    let sids: Vec<TrackSid> = pub_room
        .local_participant()
        .track_publications()
        .values()
        .map(|p| p.sid())
        .collect();
    for sid in sids {
        let _ = pub_room.local_participant().unpublish_track(&sid).await;
    }
    tokio::time::sleep(SETTLE).await;

    let discovered = discover_window_publications(&sub_room);
    let findings =
        desktop_lib::transport::reconcile::reconcile(&discovered, &receiver.tracked());
    let orphan_named = findings
        .iter()
        .any(|f| f.divergence == Divergence::Orphaned);
    let orphans: Vec<_> = findings
        .iter()
        .filter(|f| f.divergence == Divergence::Orphaned)
        .collect();
    // #630: an orphan is no longer trusted on a single sample -- a sender-side
    // republish looks identical to an ended share for one pass. Drive the real
    // two-pass path against this live room rather than the decision table
    // alone: the first pass must hold, the second must retire.
    let mut orphan_ledger = RecoveryLedger::new();
    let first_pass_at = Instant::now();
    let orphan_steps_first: Vec<_> =
        plan_recovery_steps(&findings, &receiver.tracked(), &mut orphan_ledger, first_pass_at)
            .into_iter()
            .zip(findings.iter())
            .filter(|(_, f)| f.divergence == Divergence::Orphaned)
            .map(|(step, _)| step)
            .collect();
    let orphan_steps: Vec<_> = plan_recovery_steps(
        &findings,
        &receiver.tracked(),
        &mut orphan_ledger,
        first_pass_at + ORPHANED_GRACE,
    )
    .into_iter()
    .zip(findings.iter())
    .filter(|(_, f)| f.divergence == Divergence::Orphaned)
    .map(|(step, _)| step)
    .collect();
    println!("  discovered={discovered:?}");
    println!("  findings={findings:?}");
    verdicts.push(Verdict {
        name: "4. state with no backing publication is named Orphaned",
        pass: discovered.is_empty() && orphan_named,
        detail: format!("discovered={} findings={findings:?}", discovered.len()),
    });
    verdicts.push(Verdict {
        name: "4. an orphan holds one pass for a republish in flight, then reports truth",
        pass: !orphans.is_empty()
            && orphan_steps_first
                .iter()
                .all(|step| *step == RecoveryStep::Wait)
            && orphan_steps
                .iter()
                .all(|step| *step == RecoveryStep::ReportTruth),
        detail: if orphans.is_empty() {
            "INSUFFICIENT DATA: no Orphaned finding was evaluated".to_string()
        } else {
            format!(
                "evaluated {} Orphaned finding(s): first pass={orphan_steps_first:?} \
                 second pass past ORPHANED_GRACE={orphan_steps:?}",
                orphans.len()
            )
        },
    });

    // ---- verdict -----------------------------------------------------------
    stop.store(true, Ordering::Relaxed);
    println!("\n=== verdict ===");
    let mut failed = 0;
    for verdict in &verdicts {
        println!(
            "  [{}] {}\n        {}",
            if verdict.pass { "PASS" } else { "FAIL" },
            verdict.name,
            verdict.detail
        );
        if !verdict.pass {
            failed += 1;
        }
    }
    println!(
        "\n  {} pass, {failed} fail\n",
        verdicts.len() - failed
    );

    pub_room.close().await.ok();
    sub_room.close().await.ok();
    if failed > 0 {
        std::process::exit(1);
    }
}

/// Apply the contract's decision table for real against the live room.
fn apply_recovery(room: &Room, findings: &[desktop_lib::transport::reconcile::Finding]) {
    for finding in findings {
        let step = recovery_step(&finding.divergence, None, None);
        println!("  [B] recovery step        {:?} -> {step:?}", finding.divergence);
        if step != RecoveryStep::Attempt {
            continue;
        }
        let Some(publication) = window_publication(room) else {
            continue;
        };
        // Exactly what `reconcile::attempt_resubscribe` does -- and nothing
        // else. `set_subscribed(false)` is never called here: it clears the
        // SDK's track handle with no path back (module doc hazard 1b).
        publication.set_subscribed(true);
    }
}

fn window_publication(room: &Room) -> Option<RemoteTrackPublication> {
    room.remote_participants().values().find_map(|participant| {
        participant
            .track_publications()
            .values()
            .find(|publication| publication.name() == format!("petal-window-{WINDOW_ID}"))
            .cloned()
    })
}
