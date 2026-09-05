//! #355 share-lifecycle probe: measure, against a real SFU, whether a fresh
//! window share (a) opens at the source's real size rather than a small
//! simulcast layer, and (b) survives the 6s viewer-demand downsize-hold
//! republish instead of disappearing.
//!
//! Both halves of #355 are measured, never eyeballed:
//!
//!   * **geometry** -- the probe publishes a known 1920x1080 source and
//!     records the pixel dimensions the subscriber actually decodes, so
//!     "appears too small" becomes `received != source`.
//!   * **timeline** -- presence is sampled every 100ms for the whole run and
//!     every room event is timestamped with its sid, so "disappears after a
//!     few seconds" becomes an exact `t=` at which frames stop and a named
//!     state transition.
//!
//! The republish at t=6s reproduces `session/share.rs`'s real downsize-hold
//! sequence (publish the new track first, then unpublish the old one -- both
//! named `petal-window-<id>`), which is what made the pre-fix receiver tear
//! down a live window.
//!
//! Two positive controls prove the harness can actually SEE each failure
//! before its absence is believed:
//!
//!   `--placeholder-demand`  advertise compositor's pre-first-frame 640x400
//!                           placeholder, as the code did before #355's fix,
//!                           and show the SFU hand back a small layer.
//!   `--no-sid-guard`        replace `should_remove_window` with the pre-fix
//!                           unconditional teardown and show the window die
//!                           on the republish.
//!
//! Usage (needs a LiveKit server; a local dev one is fine):
//!
//! ```sh
//! LIVEKIT_URL=ws://localhost:7889 \
//! LIVEKIT_API_KEY=devkey \
//! LIVEKIT_API_SECRET=secretsecretsecretsecretsecretsecret \
//!   cargo run --example share_lifecycle_probe -- [--seconds 35] \
//!     [--placeholder-demand] [--no-sid-guard]
//! ```
//!
//! Experiment loop for #355 work -- not cockpit apparatus and not a runtime
//! diagnostic subsystem (the project history §2.1).
//!
//! # `--late-joiner` mode (#357)
//!
//! ```sh
//!   cargo run --example share_lifecycle_probe -- --late-joiner \
//!     [--trials 8] [--legacy-subscribe]
//! ```
//!
//! Same two-peer, single-process, real-SFU setup, asking #357's question
//! instead: does a peer that connects *after* a share is already running
//! receive that share, and how fast? Per trial, in one room:
//!
//!   1. an **early** observer connects before anything is published, via
//!      the real [`RoomConnection`] path, and takes the connect-time
//!      receiver exactly as `session::join_room` does;
//!   2. a publisher connects and publishes `petal-window-357`;
//!   3. the early observer must reach frames -- this is the **in-run
//!      positive control**: it proves the observation machinery works, so a
//!      later silence is a real silence and not a broken harness;
//!   4. a **late** joiner connects, again via the real [`RoomConnection`]
//!      path, and we measure the delay from `connect()` returning to
//!      `TrackSubscribed` for window 357 and to its first decoded frame.
//!
//! `--legacy-subscribe` is the second positive control: the late joiner
//! drops the connect-time receiver and calls `room.subscribe()` afterwards,
//! which is exactly what `RoomConnection::connect` did before #364. It must
//! observe nothing, or the harness cannot see #357's failure at all and a
//! pass in the default mode means nothing.

use futures::StreamExt;
use livekit::options::{TrackPublishOptions, VideoCodec, VideoEncoding};
use livekit::prelude::*;
use livekit::track::{LocalTrack, LocalVideoTrack, RemoteTrack};
use livekit::webrtc::video_frame::{I420Buffer, VideoFrame, VideoRotation};
use livekit::webrtc::video_source::native::NativeVideoSource;
use livekit::webrtc::video_source::{RtcVideoSource, VideoResolution};
use livekit::webrtc::video_stream::native::NativeVideoStream;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The share source the probe publishes. 1080p is the size #355's report
/// used and is large enough that a quarter simulcast layer (480x270) is
/// unmistakably distinguishable from the full one.
const SOURCE_WIDTH: u32 = 1920;
const SOURCE_HEIGHT: u32 = 1080;

/// `compositor.rs`'s DEFAULT_CONTENT_WIDTH/HEIGHT -- the placeholder panel
/// size that #355 identified as poisoning viewer demand pre-first-frame.
const PLACEHOLDER_W: u32 = 640;
const PLACEHOLDER_H: u32 = 400;

/// `viewer_demand.rs`'s MAX_DEMAND_DIMENSION_PX, what the fixed code
/// advertises until the true source size is known.
const MAX_DEMAND: u32 = 4096;

/// `session/share.rs`'s VIEWER_DEMAND_DOWNSIZE_HOLD.
const DOWNSIZE_HOLD: Duration = Duration::from_secs(6);

const WINDOW_ID: u32 = 355;
const SAMPLE_INTERVAL: Duration = Duration::from_millis(100);

/// Events are printed live as they land; the recorded copy keeps the run's
/// structured trace addressable for follow-up analysis.
#[allow(dead_code)]
#[derive(Debug, Clone)]
struct Event {
    at_ms: u128,
    kind: String,
    detail: String,
}

#[derive(Debug, Clone, Copy)]
struct Sample {
    at_ms: u128,
    present: bool,
    width: u32,
    height: u32,
    frames: u64,
}

struct Recorder {
    started: Instant,
    events: Mutex<Vec<Event>>,
    samples: Mutex<Vec<Sample>>,
}

impl Recorder {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            events: Mutex::new(Vec::new()),
            samples: Mutex::new(Vec::new()),
        }
    }

    fn ms(&self) -> u128 {
        self.started.elapsed().as_millis()
    }

    fn event(&self, kind: &str, detail: impl Into<String>) {
        let at_ms = self.ms();
        let detail = detail.into();
        println!("  [t={at_ms:>6}ms] {kind:<22} {detail}");
        self.events.lock().unwrap().push(Event {
            at_ms,
            kind: kind.to_string(),
            detail,
        });
    }
}

fn token_for(identity: &str, room: &str, publish: bool, subscribe: bool) -> String {
    desktop_lib::transport::mint_access_token(identity, room, publish, subscribe)
        .unwrap_or_else(|e| {
            eprintln!("failed to mint access token: {e}");
            std::process::exit(1);
        })
}

/// Mirrors `publisher::window_publish_options`'s Full-share posture: real
/// simulcast with q/h/f layers, so the SFU genuinely has a small layer it
/// could hand back if demand asks for one. Without simulcast the geometry
/// half of #355 would be untestable.
fn publish_options() -> TrackPublishOptions {
    TrackPublishOptions {
        source: TrackSource::Screenshare,
        video_codec: VideoCodec::H264,
        simulcast: true,
        video_encoding: Some(VideoEncoding {
            max_bitrate: 4_000_000,
            max_framerate: 30.0,
        }),
        ..Default::default()
    }
}

/// Publishes one `petal-window-<id>` track and drives 30fps of real,
/// changing content into it (a moving band -- a static frame lets the
/// encoder coast and muddies layer measurement).
async fn publish_share(room: &Room, label: &str, stop: Arc<AtomicBool>) -> TrackSid {
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
            eprintln!("publish failed ({label}): {e}");
            std::process::exit(1);
        });
    let sid = publication.sid();

    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(33));
        let mut n: u32 = 0;
        while !stop.load(Ordering::Relaxed) {
            tick.tick().await;
            let mut buf = I420Buffer::new(SOURCE_WIDTH, SOURCE_HEIGHT);
            let band = (n * 7) % SOURCE_HEIGHT;
            {
                let (y, u, v) = buf.data_mut();
                // High-contrast moving band keeps every simulcast layer fed
                // with real residual, so encoders can't idle.
                for row in 0..SOURCE_HEIGHT as usize {
                    let luma = if row.abs_diff(band as usize) < 80 { 235 } else { 16 };
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

    sid
}

#[tokio::main]
async fn main() {
    env_logger::init();
    let _ = dotenvy::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/../.env"));

    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--late-joiner") {
        late_joiner::run(&args).await;
        return;
    }

    let placeholder_demand = args.iter().any(|a| a == "--placeholder-demand");
    let no_sid_guard = args.iter().any(|a| a == "--no-sid-guard");
    let seconds: u64 = args
        .iter()
        .position(|a| a == "--seconds")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(35)
        // The run must outlast the downsize-hold republish plus the 5s
        // still-flowing window the verdict inspects.
        .max(DOWNSIZE_HOLD.as_secs() + 6);

    let url = desktop_lib::transport::token::livekit_url().unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });
    let room_name = format!(
        "petal-355-probe-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    );

    let (demand_w, demand_h) = if placeholder_demand {
        (PLACEHOLDER_W, PLACEHOLDER_H)
    } else {
        (MAX_DEMAND, MAX_DEMAND)
    };

    println!("=== #355 share-lifecycle probe ===");
    println!("  room             : {room_name}");
    println!("  source           : {SOURCE_WIDTH}x{SOURCE_HEIGHT} (simulcast q/h/f)");
    println!(
        "  pre-frame demand : {demand_w}x{demand_h}{}",
        if placeholder_demand {
            "   <-- POSITIVE CONTROL: compositor placeholder (pre-#355-fix)"
        } else {
            "   (viewer_demand MAX_DEMAND_DIMENSION_PX, as fixed)"
        }
    );
    println!(
        "  teardown guard   : {}",
        if no_sid_guard {
            "DISABLED  <-- POSITIVE CONTROL: unconditional remove (pre-#355-fix)"
        } else {
            "should_remove_window (sid-guarded, as fixed)"
        }
    );
    println!("  republish at     : t={}ms (downsize-hold)", DOWNSIZE_HOLD.as_millis());
    println!("  duration         : {seconds}s\n");

    let rec = Arc::new(Recorder::new());
    let stop = Arc::new(AtomicBool::new(false));

    // ---- subscriber peer -------------------------------------------------
    let (sub_room, mut sub_events) = Room::connect(
        &url,
        &token_for("petal-355-sub", &room_name, false, true),
        RoomOptions::default(),
    )
    .await
    .unwrap_or_else(|e| {
        eprintln!("subscriber connect failed: {e}");
        std::process::exit(1);
    });
    rec.event("subscriber-connected", "");

    // Compositor-window presence, as the receiver models it.
    let window_present = Arc::new(AtomicBool::new(false));
    let frames = Arc::new(AtomicU64::new(0));
    let last_w = Arc::new(AtomicU32::new(0));
    let last_h = Arc::new(AtomicU32::new(0));
    // Mirrors subscriber.rs's WINDOW_PUBLICATIONS map, keyed the same way.
    let publications: Arc<Mutex<HashMap<u32, String>>> = Arc::new(Mutex::new(HashMap::new()));

    let ev_rec = rec.clone();
    let ev_present = window_present.clone();
    let ev_frames = frames.clone();
    let ev_w = last_w.clone();
    let ev_h = last_h.clone();
    let ev_pubs = publications.clone();
    let ev_stop = stop.clone();

    tokio::spawn(async move {
        while let Some(event) = sub_events.recv().await {
            if ev_stop.load(Ordering::Relaxed) {
                break;
            }
            match event {
                RoomEvent::TrackSubscribed {
                    track,
                    publication,
                    participant: _,
                } => {
                    let RemoteTrack::Video(video) = track else {
                        continue;
                    };
                    let Some(window_id) =
                        desktop_lib::transport::publisher::window_id_from_track_name(&video.name())
                    else {
                        continue;
                    };
                    let sid = publication.sid().to_string();
                    // Real subscriber.rs inserts the NEW publication here,
                    // BEFORE the old track's unpublish can arrive. That
                    // ordering is the whole premise of the sid guard.
                    ev_pubs.lock().unwrap().insert(window_id, sid.clone());
                    ev_present.store(true, Ordering::Relaxed);
                    ev_rec.event(
                        "TrackSubscribed",
                        format!("window={window_id} sid={sid} -> window OPEN"),
                    );

                    // Advertise viewer demand exactly as viewer_demand.rs
                    // does pre-first-frame.
                    publication.update_video_dimensions(TrackDimension(demand_w, demand_h));
                    ev_rec.event("demand-advertised", format!("{demand_w}x{demand_h}"));

                    let f = ev_frames.clone();
                    let w = ev_w.clone();
                    let h = ev_h.clone();
                    let s = ev_stop.clone();
                    tokio::spawn(async move {
                        let mut stream = NativeVideoStream::new(video.rtc_track());
                        while let Some(frame) = stream.next().await {
                            if s.load(Ordering::Relaxed) {
                                break;
                            }
                            // The decoded buffer's own dimensions ARE the
                            // simulcast layer the SFU chose for us -- this is
                            // the "too small" measurement.
                            w.store(frame.buffer.width(), Ordering::Relaxed);
                            h.store(frame.buffer.height(), Ordering::Relaxed);
                            f.fetch_add(1, Ordering::Relaxed);
                        }
                    });
                }
                RoomEvent::TrackUnpublished {
                    publication,
                    participant: _,
                } => {
                    let Some(window_id) =
                        desktop_lib::transport::publisher::window_id_from_track_name(
                            &publication.name(),
                        )
                    else {
                        continue;
                    };
                    let unpublished_sid = publication.sid().to_string();
                    let current = ev_pubs.lock().unwrap().get(&window_id).cloned();

                    // THE decision under test, fed real sids in real order.
                    let remove = if no_sid_guard {
                        true // pre-#355 behaviour
                    } else {
                        desktop_lib::transport::subscriber::should_remove_window(
                            current.as_deref(),
                            &unpublished_sid,
                        )
                    };

                    ev_rec.event(
                        "TrackUnpublished",
                        format!(
                            "window={window_id} unpublished_sid={unpublished_sid} current_sid={} -> {}",
                            current.as_deref().unwrap_or("<none>"),
                            if remove {
                                "REMOVE WINDOW"
                            } else {
                                "ignored (stale)"
                            }
                        ),
                    );

                    if remove {
                        ev_pubs.lock().unwrap().remove(&window_id);
                        ev_present.store(false, Ordering::Relaxed);
                        ev_rec.event("window-removed", "compositor window torn down");
                    }
                }
                _ => {}
            }
        }
    });

    // ---- publisher peer --------------------------------------------------
    let (pub_room, mut pub_events) = Room::connect(
        &url,
        &token_for("petal-355-pub", &room_name, true, false),
        RoomOptions::default(),
    )
    .await
    .unwrap_or_else(|e| {
        eprintln!("publisher connect failed: {e}");
        std::process::exit(1);
    });
    tokio::spawn(async move { while pub_events.recv().await.is_some() {} });
    rec.event("publisher-connected", "");

    let first_sid = publish_share(&pub_room, "initial", stop.clone()).await;
    rec.event(
        "published",
        format!("sid={first_sid} {SOURCE_WIDTH}x{SOURCE_HEIGHT}"),
    );

    // ---- presence/geometry sampler --------------------------------------
    let s_rec = rec.clone();
    let s_present = window_present.clone();
    let s_frames = frames.clone();
    let s_w = last_w.clone();
    let s_h = last_h.clone();
    let s_stop = stop.clone();
    let sampler = tokio::spawn(async move {
        let mut tick = tokio::time::interval(SAMPLE_INTERVAL);
        while !s_stop.load(Ordering::Relaxed) {
            tick.tick().await;
            let sample = Sample {
                at_ms: s_rec.ms(),
                present: s_present.load(Ordering::Relaxed),
                width: s_w.load(Ordering::Relaxed),
                height: s_h.load(Ordering::Relaxed),
                frames: s_frames.load(Ordering::Relaxed),
            };
            s_rec.samples.lock().unwrap().push(sample);
        }
    });

    // ---- the downsize-hold republish ------------------------------------
    tokio::time::sleep(DOWNSIZE_HOLD).await;
    rec.event(
        "republish-begin",
        "share.rs downsize-hold: publish NEW then unpublish OLD",
    );
    let second_sid = publish_share(&pub_room, "republished", stop.clone()).await;
    rec.event("published", format!("sid={second_sid} (new)"));
    pub_room
        .local_participant()
        .unpublish_track(&first_sid)
        .await
        .ok();
    rec.event("unpublished", format!("sid={first_sid} (old)"));

    tokio::time::sleep(Duration::from_secs(seconds) - DOWNSIZE_HOLD).await;
    stop.store(true, Ordering::Relaxed);
    sampler.abort();

    // ---- verdict ---------------------------------------------------------
    let samples = rec.samples.lock().unwrap().clone();
    report(&samples, placeholder_demand, no_sid_guard, seconds);

    sub_room.close().await.ok();
    pub_room.close().await.ok();
}

fn report(samples: &[Sample], placeholder_demand: bool, no_sid_guard: bool, seconds: u64) {
    println!("\n=== timeline (presence + decoded geometry) ===");
    let mut last_key = (false, 0u32, 0u32);
    for s in samples {
        let key = (s.present, s.width, s.height);
        if key != last_key {
            println!(
                "  t={:>6}ms  present={:<5} decoded={}x{}  frames={}",
                s.at_ms, s.present, s.width, s.height, s.frames
            );
            last_key = key;
        }
    }

    // --- geometry half ---
    let settled: Vec<&Sample> = samples
        .iter()
        .filter(|s| s.at_ms > 3000 && s.width > 0)
        .collect();
    let geom_ok = settled
        .last()
        .map(|s| s.width == SOURCE_WIDTH && s.height == SOURCE_HEIGHT);

    println!("\n=== HALF 1: \"appears too small\" (geometry) ===");
    println!("  source   : {SOURCE_WIDTH}x{SOURCE_HEIGHT}");
    match settled.last() {
        Some(s) => {
            let ratio = f64::from(s.width) / f64::from(SOURCE_WIDTH);
            println!(
                "  received : {}x{}  ({:.0}% of source long edge)",
                s.width, s.height, ratio * 100.0
            );
        }
        None => println!("  received : <no frames decoded>"),
    }
    match geom_ok {
        Some(true) => println!("  VERDICT  : PASS -- received size equals source size"),
        Some(false) => println!("  VERDICT  : FAIL -- received smaller than source (#355 half 1)"),
        None => println!("  VERDICT  : INCONCLUSIVE -- no frames"),
    }

    // --- timeline half ---
    let end_ms = (u128::from(seconds) * 1000).saturating_sub(5000);
    let disappeared_at = samples
        .iter()
        .skip_while(|s| !s.present)
        .find(|s| !s.present)
        .map(|s| s.at_ms);
    let final_present = samples.last().map(|s| s.present).unwrap_or(false);
    // Frames must still be arriving at the end, not merely "present".
    let late: Vec<&Sample> = samples.iter().filter(|s| s.at_ms > end_ms).collect();
    let still_flowing = match (late.first(), late.last()) {
        (Some(a), Some(b)) => b.frames > a.frames,
        _ => false,
    };

    println!("\n=== HALF 2: \"disappears within ~6 seconds\" (timeline) ===");
    match disappeared_at {
        Some(t) => println!("  window removed at t={t}ms"),
        None => println!("  window never removed for the full {seconds}s"),
    }
    println!("  present at end     : {final_present}");
    println!("  frames still flowing in last 5s: {still_flowing}");
    let timeline_ok = disappeared_at.is_none() && final_present && still_flowing;
    println!(
        "  VERDICT  : {}",
        if timeline_ok {
            "PASS -- survived the republish, frames still arriving"
        } else {
            "FAIL -- share disappeared or stalled (#355 half 2)"
        }
    );

    // --- overall, with control expectations ---
    let control = placeholder_demand || no_sid_guard;
    println!("\n=== RESULT ===");
    if control {
        println!("  positive-control run: the harness is expected to REPORT FAILURE here.");
        println!(
            "  placeholder-demand={placeholder_demand} no-sid-guard={no_sid_guard} -> geometry_ok={geom_ok:?} timeline_ok={timeline_ok}"
        );
        if geom_ok == Some(false) || !timeline_ok {
            println!("  CONTROL OK -- harness detects the pre-fix failure.");
        } else {
            println!("  CONTROL DID NOT TRIP -- harness cannot see this failure; do not trust a pass.");
            std::process::exit(2);
        }
    } else if geom_ok == Some(true) && timeline_ok {
        println!("  PASS -- #355 does not reproduce on this build.");
    } else {
        println!("  FAIL -- #355 reproduces.");
        std::process::exit(1);
    }
}

/// #357: does a peer that joins *after* a share already started receive it?
///
/// Measured through the real [`desktop_lib::transport::RoomConnection`] path
/// -- the same `connect()` + `take_compositor_events()` pair that
/// `session::join_room` hands to `subscriber::start_compositor_feed` -- so a
/// pass here is a statement about the app's own join sequence rather than
/// about a re-registered stand-in receiver.
mod late_joiner {
    use super::{publish_share, token_for};
    use futures::StreamExt;
    use livekit::prelude::*;
    use livekit::track::RemoteTrack;
    use livekit::webrtc::video_stream::native::NativeVideoStream;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// How long a joiner is given to surface an already-running share before
    /// the trial is scored as a miss. The share is live and the SFU already
    /// has it in the join offer, so this is generous by two orders of
    /// magnitude, not a tight race.
    const DISCOVERY_DEADLINE: Duration = Duration::from_secs(10);

    /// Time the share is left running before the late joiner connects, so it
    /// is unambiguously "already in progress" rather than concurrent.
    const SHARE_SETTLE: Duration = Duration::from_secs(5);

    /// Stand-in for `session::join_room`'s pre-`start_compositor_feed` work
    /// (the identity-palette signaling round trip and the telepointer /
    /// remote-control / draw / latency / stats / cockpit / viewer-demand
    /// wiring). Only the legacy control needs it: it is the window during
    /// which the pre-#364 code had already dropped the connect-time receiver
    /// but had not yet registered its replacement.
    const LEGACY_JOIN_TAIL: Duration = Duration::from_millis(150);

    /// What one joiner observed, relative to its own `connect()` returning.
    struct Observation {
        /// `Connected` carried the already-published window in its
        /// `participants_with_tracks` snapshot.
        snapshot_had_window: Arc<AtomicBool>,
        subscribed_ms: Arc<AtomicU64>,
        first_frame_ms: Arc<AtomicU64>,
        frames: Arc<AtomicU64>,
    }

    const NOT_SEEN: u64 = u64::MAX;

    impl Observation {
        fn new() -> Self {
            Self {
                snapshot_had_window: Arc::new(AtomicBool::new(false)),
                subscribed_ms: Arc::new(AtomicU64::new(NOT_SEEN)),
                first_frame_ms: Arc::new(AtomicU64::new(NOT_SEEN)),
                frames: Arc::new(AtomicU64::new(0)),
            }
        }

        fn saw_share(&self) -> bool {
            self.frames.load(Ordering::Relaxed) > 0
        }
    }

    fn ms(v: u64) -> String {
        if v == NOT_SEEN {
            "never".to_string()
        } else {
            format!("{v}ms")
        }
    }

    /// Drive one joiner's event receiver, recording when the already-active
    /// window share becomes visible to it. Mirrors the decisions
    /// `subscriber::start_compositor_feed` makes on the same events: filter
    /// to video tracks whose name parses as a window id, then pull frames
    /// off a `NativeVideoStream`. Frames -- not the event alone -- are what
    /// count as "the window rendered", since an event with no media behind
    /// it would still leave the user staring at nothing.
    fn observe(
        mut events: tokio::sync::mpsc::UnboundedReceiver<RoomEvent>,
        since: Instant,
        stop: Arc<AtomicBool>,
    ) -> Observation {
        let obs = Observation::new();
        let snapshot_had_window = obs.snapshot_had_window.clone();
        let subscribed_ms = obs.subscribed_ms.clone();
        let first_frame_ms = obs.first_frame_ms.clone();
        let frames = obs.frames.clone();

        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                match event {
                    // Recorded for diagnosis only. `start_compositor_feed`
                    // has no `Connected` arm, so if this were ever the ONLY
                    // carrier of an already-active share, the app would miss
                    // it even with the connect-time receiver threaded
                    // through -- which is precisely the residual risk this
                    // probe exists to rule in or out.
                    RoomEvent::Connected {
                        participants_with_tracks,
                    } => {
                        let has = participants_with_tracks.iter().any(|(_, pubs)| {
                            pubs.iter().any(|p| {
                                desktop_lib::transport::publisher::window_id_from_track_name(
                                    &p.name(),
                                )
                                .is_some()
                            })
                        });
                        snapshot_had_window.store(has, Ordering::Relaxed);
                    }
                    RoomEvent::TrackSubscribed {
                        track,
                        publication: _,
                        participant: _,
                    } => {
                        let RemoteTrack::Video(video) = track else {
                            continue;
                        };
                        if desktop_lib::transport::publisher::window_id_from_track_name(
                            &video.name(),
                        )
                        .is_none()
                        {
                            continue;
                        }
                        let _ = subscribed_ms.compare_exchange(
                            NOT_SEEN,
                            since.elapsed().as_millis() as u64,
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        );

                        let ff = first_frame_ms.clone();
                        let f = frames.clone();
                        let s = stop.clone();
                        tokio::spawn(async move {
                            let mut stream = NativeVideoStream::new(video.rtc_track());
                            while stream.next().await.is_some() {
                                if s.load(Ordering::Relaxed) {
                                    break;
                                }
                                let _ = ff.compare_exchange(
                                    NOT_SEEN,
                                    since.elapsed().as_millis() as u64,
                                    Ordering::Relaxed,
                                    Ordering::Relaxed,
                                );
                                f.fetch_add(1, Ordering::Relaxed);
                            }
                        });
                    }
                    _ => {}
                }
            }
        });

        obs
    }

    /// One trial. Returns `(early_control, late)` observations.
    async fn trial(
        url: &str,
        room_name: &str,
        legacy_subscribe: bool,
        stop: Arc<AtomicBool>,
    ) -> (Observation, Observation, Arc<desktop_lib::transport::RoomConnection>) {
        // --- early observer: connects BEFORE any publish -------------------
        let early_conn = desktop_lib::transport::RoomConnection::connect(
            url,
            &token_for("petal-357-early", room_name, false, true),
        )
        .await
        .unwrap_or_else(|e| {
            eprintln!("early observer connect failed: {e}");
            std::process::exit(1);
        });
        let early_started = Instant::now();
        let early = observe(
            early_conn
                .take_compositor_events()
                .expect("connect-time receiver present"),
            early_started,
            stop.clone(),
        );

        // --- publisher ------------------------------------------------------
        let (pub_room, mut pub_events) = Room::connect(
            url,
            &token_for("petal-357-pub", room_name, true, false),
            RoomOptions::default(),
        )
        .await
        .unwrap_or_else(|e| {
            eprintln!("publisher connect failed: {e}");
            std::process::exit(1);
        });
        let pub_stop = stop.clone();
        tokio::spawn(async move {
            while pub_events.recv().await.is_some() {
                if pub_stop.load(Ordering::Relaxed) {
                    break;
                }
            }
        });
        publish_share(&pub_room, "late-joiner-source", stop.clone()).await;

        // Let the share genuinely settle, so the late joiner is joining an
        // established share rather than racing the publish.
        tokio::time::sleep(SHARE_SETTLE).await;

        // --- late joiner ----------------------------------------------------
        let late_conn = Arc::new(
            desktop_lib::transport::RoomConnection::connect(
                url,
                &token_for("petal-357-late", room_name, false, true),
            )
            .await
            .unwrap_or_else(|e| {
                eprintln!("late joiner connect failed: {e}");
                std::process::exit(1);
            }),
        );
        let late_started = Instant::now();

        let late = if legacy_subscribe {
            // POSITIVE CONTROL -- reproduce pre-#364 `RoomConnection::connect`:
            // discard the receiver `Room::connect` registered, do the join
            // tail's work, and only then register a fresh one.
            late_conn.discard_compositor_events();
            tokio::time::sleep(LEGACY_JOIN_TAIL).await;
            observe(late_conn.room().subscribe(), late_started, stop.clone())
        } else {
            observe(
                late_conn
                    .take_compositor_events()
                    .expect("connect-time receiver present"),
                late_started,
                stop.clone(),
            )
        };

        // Give the late joiner its full deadline, but stop early once it has
        // real frames so a healthy run is not padded to 10s per trial.
        let deadline = Instant::now() + DISCOVERY_DEADLINE;
        while Instant::now() < deadline {
            if late.frames.load(Ordering::Relaxed) > 5 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        pub_room.close().await.ok();
        early_conn.room().close().await.ok();

        (early, late, late_conn)
    }

    pub async fn run(args: &[String]) {
        let legacy_subscribe = args.iter().any(|a| a == "--legacy-subscribe");
        let trials: u32 = args
            .iter()
            .position(|a| a == "--trials")
            .and_then(|i| args.get(i + 1))
            .and_then(|s| s.parse().ok())
            .unwrap_or(8);

        let url = desktop_lib::transport::token::livekit_url().unwrap_or_else(|e| {
            eprintln!("{e}");
            std::process::exit(1);
        });

        println!("=== #357 late-joiner probe ===");
        println!("  trials           : {trials}");
        println!("  share settles for: {SHARE_SETTLE:?} before the late joiner connects");
        println!("  discovery budget : {DISCOVERY_DEADLINE:?}");
        println!(
            "  late-joiner path : {}\n",
            if legacy_subscribe {
                "discard connect receiver + late room.subscribe()   <-- POSITIVE CONTROL (pre-#364)"
            } else {
                "RoomConnection::take_compositor_events()   (as session::join_room does)"
            }
        );

        let mut late_seen = 0u32;
        let mut control_seen = 0u32;
        let mut subscribe_latencies = Vec::new();
        let mut frame_latencies = Vec::new();
        let mut snapshot_carried = 0u32;

        for t in 1..=trials {
            let stop = Arc::new(AtomicBool::new(false));
            let room_name = format!(
                "petal-357-{}-{t}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis()
            );
            let (early, late, late_conn) =
                trial(&url, &room_name, legacy_subscribe, stop.clone()).await;

            let early_ok = early.saw_share();
            let late_ok = late.saw_share();
            if early_ok {
                control_seen += 1;
            }
            if late_ok {
                late_seen += 1;
                subscribe_latencies.push(late.subscribed_ms.load(Ordering::Relaxed));
                frame_latencies.push(late.first_frame_ms.load(Ordering::Relaxed));
            }
            if late.snapshot_had_window.load(Ordering::Relaxed) {
                snapshot_carried += 1;
            }

            println!(
                "  trial {t:>2}: early(control) frames={:<5} | late frames={:<5} \
                 subscribed={:<8} first_frame={:<8} connected-snapshot_had_window={}",
                early.frames.load(Ordering::Relaxed),
                late.frames.load(Ordering::Relaxed),
                ms(late.subscribed_ms.load(Ordering::Relaxed)),
                ms(late.first_frame_ms.load(Ordering::Relaxed)),
                late.snapshot_had_window.load(Ordering::Relaxed),
            );

            stop.store(true, Ordering::Relaxed);
            late_conn.room().close().await.ok();
            tokio::time::sleep(Duration::from_millis(300)).await;
        }

        let mean = |v: &[u64]| -> String {
            if v.is_empty() {
                "n/a".to_string()
            } else {
                format!("{}ms", v.iter().sum::<u64>() / v.len() as u64)
            }
        };
        let worst = |v: &[u64]| -> String {
            v.iter().max().map(|m| format!("{m}ms")).unwrap_or_else(|| "n/a".to_string())
        };

        println!("\n=== RESULT ===");
        println!("  in-run positive control (early joiner saw the share): {control_seen}/{trials}");
        println!("  late joiner saw the already-active share           : {late_seen}/{trials}");
        println!(
            "  late TrackSubscribed latency  mean={} worst={}",
            mean(&subscribe_latencies),
            worst(&subscribe_latencies)
        );
        println!(
            "  late first-frame latency      mean={} worst={}",
            mean(&frame_latencies),
            worst(&frame_latencies)
        );
        println!(
            "  Connected snapshot carried the window in {snapshot_carried}/{trials} trials"
        );

        if control_seen < trials {
            println!(
                "\n  HARNESS INVALID -- the early joiner, which cannot be affected by #357,\n  \
                 failed to see the share in {}/{trials} trials. Nothing here is interpretable.",
                trials - control_seen
            );
            std::process::exit(3);
        }

        if legacy_subscribe {
            // #357 is a RACE on the `TrackSubscribed` half, not a certainty:
            // the pre-#364 path lost by a signaling round trip, and on a
            // loopback SFU that round trip is short enough that the late
            // `room.subscribe()` occasionally still wins. So the control
            // trips on "demonstrably misses", not "never succeeds" --
            // demanding 0/N would be measuring loopback latency, not the bug.
            //
            // The `Connected` snapshot half IS deterministic and is reported
            // separately: a receiver registered after connect can never see
            // it, so `snapshot_carried` should be 0/N here and N/N in the
            // default mode. That difference has no race in it at all.
            let misses = trials - late_seen;
            if misses > 0 {
                println!(
                    "\n  CONTROL OK -- the pre-#364 path missed an already-active share in\n  \
                     {misses}/{trials} trials, so this harness demonstrably observes #357's\n  \
                     failure and a default-mode pass is meaningful."
                );
                if snapshot_carried == 0 {
                    println!(
                        "  The deterministic half is confirmed too: the connect-time `Connected`\n  \
                         snapshot reached this path in 0/{trials} trials."
                    );
                }
            } else {
                println!(
                    "\n  CONTROL DID NOT TRIP -- the pre-#364 path saw the share in all\n  \
                     {trials} trials; this harness cannot demonstrate #357.\n  \
                     Do not trust a default-mode pass."
                );
                std::process::exit(2);
            }
        } else if late_seen == trials {
            println!("\n  PASS -- #357 does not reproduce on this build.");
        } else {
            println!("\n  FAIL -- #357 reproduces: the late joiner missed an already-active share.");
            std::process::exit(1);
        }
    }
}
