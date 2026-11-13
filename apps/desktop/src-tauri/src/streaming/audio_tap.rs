//! Meeting audio for client mode: a `NativeAudioStream` sink on every audio
//! track in the room -- local app/system audio and every subscribed remote
//! track -- feeding the stream's [`AudioMixer`] and the echo reference of
//! the stream's own mic capture ([`super::mic`]): all of it is what Petal
//! plays out (or the shared app plays itself), so it is what the mic may
//! pick up. The local mic track is skipped: it is fed by the platform audio
//! device module, which never reaches a track sink, so [`super::mic`]
//! captures the mic instead. Nothing is played locally, so the sharer never
//! hears a duplicate. A muted track delivers silence (or nothing), which the
//! mixer treats the same.
//!
//! A participant who keeps their audio off live streams (`petal.stream-optout`
//! = `"1"`) still feeds the echo reference -- the mic still hears them -- but
//! never the stream mix: their sources leave the mix the moment they opt
//! out and rejoin when they opt back in. Only remote participants can opt
//! out; the streamer's own setting doesn't apply to their own stream.
//!
//! Consent comes before audio: no remote participant is heard on the stream
//! until the room has been told it is live (our `petal.live` accepted by the
//! server) and [`REMOTE_AUDIO_GRACE`] has passed, so everyone sees LIVE and
//! can opt out first. Someone who joins (or rejoins) mid-stream waits the
//! same grace from their own arrival. If the announcement is lost (a full
//! reconnect drops it), remote audio leaves the stream until it is back and
//! the grace has passed again. The streamer's own audio is not gated: going
//! live is their consent.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use livekit::prelude::{LocalTrack, Participant, RemoteTrack, Room, RoomEvent, TrackSource};
use livekit::webrtc::audio_stream::native::NativeAudioStream;
use livekit::webrtc::prelude::RtcAudioTrack;
use tokio_util::sync::CancellationToken;

use super::mixer::{AudioMixer, MIX_CHANNELS, MIX_SAMPLE_RATE};
use super::wire::{is_opted_out, STREAM_OPTOUT_ATTRIBUTE};
use crate::sync_ext::MutexExt;

/// How long remote audio stays off a stream after the room learns it is live
/// (or after a participant arrives mid-stream). Pinned by
/// contracts/petal-contracts.json `streamOptOutAttribute.remoteAudioGraceMs`.
pub const REMOTE_AUDIO_GRACE: Duration = Duration::from_secs(5);

/// How often the consent gate is re-checked (the grace runs out between
/// room events).
const CONSENT_TICK: Duration = Duration::from_millis(250);

/// Whether the room has been told this stream is live: our `petal.live`
/// accepted by the server.
pub type Announced = Arc<dyn Fn() -> bool + Send + Sync>;

/// When each remote participant may be heard on the stream.
#[derive(Debug, Default)]
struct Consent {
    /// When the announcement was first seen accepted (reset if lost).
    announced_at: Option<Instant>,
    /// When each participant arrived during this stream.
    arrived_at: HashMap<String, Instant>,
}

impl Consent {
    fn observe_announced(&mut self, announced: bool, now: Instant) {
        if !announced {
            self.announced_at = None;
        } else if self.announced_at.is_none() {
            self.announced_at = Some(now);
        }
    }

    fn arrived(&mut self, identity: &str, now: Instant) {
        self.arrived_at.insert(identity.to_string(), now);
    }

    /// Whether `identity` has had [`REMOTE_AUDIO_GRACE`] to see LIVE.
    fn admits(&self, identity: &str, now: Instant) -> bool {
        let Some(announced_at) = self.announced_at else { return false };
        let since = self.arrived_at.get(identity).map_or(announced_at, |arrived| announced_at.max(*arrived));
        now.saturating_duration_since(since) >= REMOTE_AUDIO_GRACE
    }
}

/// The stream mix and the stream mic's echo reference.
#[derive(Clone)]
struct Mixes {
    stream: Arc<Mutex<AudioMixer>>,
    reference: Arc<Mutex<AudioMixer>>,
}

impl Mixes {
    /// One frame from source `sid`: always into the echo reference, into the
    /// stream only while its owner is included. The flag is read under the
    /// stream mix's lock, the same lock [`Mixes::set_in_stream`] flips it
    /// under, so a frame can never land after an opt-out removed the source.
    fn route(&self, sid: &str, pcm: &[i16], channels: usize, in_stream: &AtomicBool) {
        self.reference.lock_unpoisoned().push(sid, pcm, channels);
        let mut stream = self.stream.lock_unpoisoned();
        if in_stream.load(Ordering::Relaxed) {
            stream.push(sid, pcm, channels);
        }
    }

    /// Include source `sid` in the stream mix, or drop it from it now.
    fn set_in_stream(&self, sid: &str, in_stream: &AtomicBool, include: bool) {
        let mut stream = self.stream.lock_unpoisoned();
        in_stream.store(include, Ordering::Relaxed);
        if !include {
            stream.remove_source(sid);
        }
    }
}

struct Tap {
    cancel: CancellationToken,
    /// The remote participant whose track this is; `None` for our own.
    owner: Option<String>,
    in_stream: Arc<AtomicBool>,
}

struct Taps {
    mixes: Mixes,
    taps: HashMap<String, Tap>,
    /// Remote participants who keep their audio off live streams.
    opted_out: HashSet<String>,
    consent: Consent,
    /// The room is reconnecting: whatever was announced may be gone.
    reconnecting: bool,
}

impl Taps {
    fn new(mixes: Mixes) -> Self {
        Self { mixes, taps: HashMap::new(), opted_out: HashSet::new(), consent: Consent::default(), reconnecting: false }
    }

    /// Whether `owner`'s audio belongs in the stream now. Our own (`None`)
    /// always does.
    fn includes(&self, owner: Option<&str>, now: Instant) -> bool {
        owner.is_none_or(|owner| !self.opted_out.contains(owner) && self.consent.admits(owner, now))
    }

    /// Bring every tap in line with the opt-outs and the consent gate.
    fn reevaluate(&mut self, now: Instant) {
        for (sid, tap) in &self.taps {
            let include = self.includes(tap.owner.as_deref(), now);
            if tap.in_stream.load(Ordering::Relaxed) != include {
                if tap.owner.is_some() {
                    log::info!(
                        "streaming: {} {} the stream",
                        tap.owner.as_deref().unwrap_or_default(),
                        if include { "joins" } else { "leaves" }
                    );
                }
                self.mixes.set_in_stream(sid, &tap.in_stream, include);
            }
        }
    }

    /// The consent gate's inputs at `now`.
    fn tick(&mut self, announced: bool, now: Instant) {
        self.consent.observe_announced(announced && !self.reconnecting, now);
        self.reevaluate(now);
    }

    fn attach(
        &mut self,
        sid: String,
        owner: Option<String>,
        track: RtcAudioTrack,
        parent: &CancellationToken,
    ) {
        if self.taps.contains_key(&sid) {
            return;
        }
        let label = owner.as_deref().map_or_else(|| "shared app audio".to_string(), |o| format!("remote audio from {o}"));
        let include = self.includes(owner.as_deref(), Instant::now());
        log::info!(
            "streaming: stream audio + {label} ({sid}){}",
            if include { "" } else { " (not on the stream yet)" }
        );
        let cancel = parent.child_token();
        let in_stream = Arc::new(AtomicBool::new(include));
        self.taps.insert(sid.clone(), Tap { cancel: cancel.clone(), owner, in_stream: in_stream.clone() });
        let mixes = self.mixes.clone();
        tokio::spawn(async move {
            let mut stream = NativeAudioStream::new(track, MIX_SAMPLE_RATE as i32, MIX_CHANNELS as i32);
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    frame = stream.next() => {
                        let Some(frame) = frame else { break };
                        mixes.route(&sid, &frame.data, frame.num_channels as usize, &in_stream);
                    }
                }
            }
            mixes.stream.lock_unpoisoned().remove_source(&sid);
            mixes.reference.lock_unpoisoned().remove_source(&sid);
        });
    }

    fn detach(&mut self, sid: &str) {
        if let Some(tap) = self.taps.remove(sid) {
            log::info!("streaming: stream audio - {sid}");
            // Out of the stream now: the reader task stops asynchronously and
            // must not leave queued audio behind.
            self.mixes.set_in_stream(sid, &tap.in_stream, false);
            tap.cancel.cancel();
        }
    }

    /// A remote participant's `petal.stream-optout` reads `value`. `"1"` opts
    /// them out; another present value (`"0"`) opts them back in; absent
    /// changes nothing: a participant seen opted out who reappears without
    /// the attribute (a rejoin or full reconnect lost it) stays out.
    fn observe_opt_out(&mut self, identity: &str, value: Option<&str>) {
        match value {
            None => {}
            value => self.set_opted_out(identity, is_opted_out(value)),
        }
    }

    /// A remote participant's `petal.stream-optout` is now `opted_out`.
    fn set_opted_out(&mut self, identity: &str, opted_out: bool) {
        let changed = if opted_out {
            self.opted_out.insert(identity.to_string())
        } else {
            self.opted_out.remove(identity)
        };
        if !changed {
            return;
        }
        log::info!(
            "streaming: {identity} {} live streams",
            if opted_out { "keeps their audio off" } else { "is included in" }
        );
        self.reevaluate(Instant::now());
    }

    fn cancel_all(&mut self) {
        for (_, tap) in self.taps.drain() {
            tap.cancel.cancel();
        }
    }
}

fn remote_opt_out(participant: &livekit::prelude::RemoteParticipant) -> Option<String> {
    participant.attributes().get(STREAM_OPTOUT_ATTRIBUTE).cloned()
}

/// Which local tracks the stream taps: everything but the mic.
fn taps_local(source: TrackSource) -> bool {
    source != TrackSource::Microphone
}

fn local_rtc(track: &LocalTrack) -> Option<RtcAudioTrack> {
    match track {
        LocalTrack::Audio(audio) => Some(audio.rtc_track()),
        _ => None,
    }
}

fn remote_rtc(track: &RemoteTrack) -> Option<RtcAudioTrack> {
    match track {
        RemoteTrack::Audio(audio) => Some(audio.rtc_track()),
        _ => None,
    }
}

/// Tap the room's audio into `mixer` and the stream mic's echo `reference`
/// until `cancel` fires.
/// Remote audio joins the stream only once `announced` holds (see the
/// module doc).
pub fn start(
    room: Arc<Room>,
    mixer: Arc<Mutex<AudioMixer>>,
    reference: Arc<Mutex<AudioMixer>>,
    announced: Announced,
    cancel: CancellationToken,
) {
    let mut taps = Taps::new(Mixes { stream: mixer, reference });
    // Subscribe before the snapshot so nothing published in between is missed;
    // `attach` ignores duplicates.
    let mut events = room.subscribe();
    tokio::spawn(async move {
        // Opt-outs first, so no opted-out track is ever fed in.
        for (identity, participant) in room.remote_participants() {
            if is_opted_out(participant.attributes().get(STREAM_OPTOUT_ATTRIBUTE).map(String::as_str)) {
                taps.set_opted_out(&identity.to_string(), true);
            }
        }
        for (sid, publication) in room.local_participant().track_publications() {
            if !taps_local(publication.source()) {
                continue;
            }
            if let Some(track) = publication.track().as_ref().and_then(local_rtc) {
                taps.attach(sid.to_string(), None, track, &cancel);
            }
        }
        for (identity, participant) in room.remote_participants() {
            for (sid, publication) in participant.track_publications() {
                if let Some(track) = publication.track().as_ref().and_then(remote_rtc) {
                    taps.attach(sid.to_string(), Some(identity.to_string()), track, &cancel);
                }
            }
        }
        let mut consent_tick = tokio::time::interval(CONSENT_TICK);
        loop {
            let event = tokio::select! {
                () = cancel.cancelled() => break,
                _ = consent_tick.tick() => {
                    taps.tick(announced(), Instant::now());
                    continue;
                }
                event = events.recv() => match event { Some(event) => event, None => break },
            };
            match event {
                RoomEvent::ParticipantConnected(participant) => {
                    let identity = participant.identity().to_string();
                    taps.consent.arrived(&identity, Instant::now());
                    // A (re)joining participant may arrive already opted out.
                    taps.observe_opt_out(&identity, remote_opt_out(&participant).as_deref());
                    taps.reevaluate(Instant::now());
                }
                // A full reconnect may have lost our announcement: remote audio
                // leaves the stream until it is written again (and the grace
                // has passed again).
                RoomEvent::Reconnecting => {
                    taps.reconnecting = true;
                    taps.tick(false, Instant::now());
                }
                RoomEvent::Reconnected => taps.reconnecting = false,
                RoomEvent::TrackSubscribed { track, publication, participant } => {
                    if let Some(rtc) = remote_rtc(&track) {
                        let owner = participant.identity().to_string();
                        taps.observe_opt_out(&owner, remote_opt_out(&participant).as_deref());
                        taps.attach(publication.sid().to_string(), Some(owner), rtc, &cancel);
                    }
                }
                RoomEvent::TrackUnsubscribed { publication, .. } => {
                    taps.detach(&publication.sid().to_string());
                }
                RoomEvent::ParticipantAttributesChanged { participant: Participant::Remote(participant), .. } => {
                    taps.observe_opt_out(&participant.identity().to_string(), remote_opt_out(&participant).as_deref());
                }
                RoomEvent::LocalTrackPublished { publication, track, .. } => {
                    if let Some(rtc) = local_rtc(&track).filter(|_| taps_local(publication.source())) {
                        taps.attach(publication.sid().to_string(), None, rtc, &cancel);
                    }
                }
                RoomEvent::LocalTrackRepublished { previous_sid, publication, track, .. } => {
                    taps.detach(&previous_sid.to_string());
                    if let Some(rtc) = local_rtc(&track).filter(|_| taps_local(publication.source())) {
                        taps.attach(publication.sid().to_string(), None, rtc, &cancel);
                    }
                }
                RoomEvent::LocalTrackUnpublished { publication, .. } => {
                    taps.detach(&publication.sid().to_string());
                }
                RoomEvent::Disconnected { .. } => break,
                _ => {}
            }
        }
        taps.cancel_all();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streaming::mixer::PRIME_FRAMES;

    #[test]
    fn the_local_mic_track_is_left_to_the_stream_mic() {
        assert!(!taps_local(TrackSource::Microphone));
        assert!(taps_local(TrackSource::ScreenshareAudio));
    }

    fn mixes() -> Mixes {
        Mixes {
            stream: Arc::new(Mutex::new(AudioMixer::new())),
            reference: Arc::new(Mutex::new(AudioMixer::with_prime(0))),
        }
    }

    fn block(value: i16) -> Vec<i16> {
        vec![value; PRIME_FRAMES * MIX_CHANNELS]
    }

    /// Taps whose stream was announced well past the grace period.
    fn announced_long_ago(mixes: Mixes) -> Taps {
        let mut taps = Taps::new(mixes);
        let past = Instant::now().checked_sub(REMOTE_AUDIO_GRACE * 2).expect("a monotonic clock this old");
        taps.consent.observe_announced(true, past);
        taps
    }

    fn insert_tap(taps: &mut Taps, sid: &str, owner: Option<&str>) -> Arc<AtomicBool> {
        let in_stream = Arc::new(AtomicBool::new(false));
        taps.taps.insert(
            sid.into(),
            Tap { cancel: CancellationToken::new(), owner: owner.map(Into::into), in_stream: in_stream.clone() },
        );
        in_stream
    }

    #[test]
    fn the_grace_period_is_pinned_to_the_contract() {
        let contracts: serde_json::Value =
            serde_json::from_str(include_str!("../../../../../contracts/petal-contracts.json")).unwrap();
        assert_eq!(
            contracts["streamOptOutAttribute"]["remoteAudioGraceMs"].as_u64(),
            Some(REMOTE_AUDIO_GRACE.as_millis() as u64)
        );
    }

    #[test]
    fn no_remote_audio_reaches_the_stream_before_the_room_is_told_and_the_grace_passes() {
        let mixes = mixes();
        let mut taps = Taps::new(mixes.clone());
        let bob = insert_tap(&mut taps, "TR_bob", Some("bob"));
        let ours = insert_tap(&mut taps, "TR_ours", None);
        let t0 = Instant::now();
        // Live, but petal.live not accepted yet: Bob is only in the echo reference.
        taps.tick(false, t0);
        assert!(!bob.load(Ordering::Relaxed));
        assert!(ours.load(Ordering::Relaxed), "the streamer's own audio is never gated");
        mixes.route("TR_bob", &block(1000), MIX_CHANNELS, &bob);
        assert_eq!(mixes.stream.lock_unpoisoned().source_count(), 0);
        assert!(mixes.reference.lock_unpoisoned().pull(480).iter().all(|s| *s == 1000));
        // Announced at t0+1s: still off for the grace period.
        taps.tick(true, t0 + Duration::from_secs(1));
        taps.tick(true, t0 + Duration::from_secs(1) + REMOTE_AUDIO_GRACE - Duration::from_millis(1));
        assert!(!bob.load(Ordering::Relaxed));
        mixes.route("TR_bob", &block(1000), MIX_CHANNELS, &bob);
        assert_eq!(mixes.stream.lock_unpoisoned().source_count(), 0);
        // Grace over: Bob is heard.
        taps.tick(true, t0 + Duration::from_secs(1) + REMOTE_AUDIO_GRACE);
        assert!(bob.load(Ordering::Relaxed));
        mixes.route("TR_bob", &block(1000), MIX_CHANNELS, &bob);
        assert!(mixes.stream.lock_unpoisoned().pull_block().iter().all(|s| *s == 1000));
    }

    #[test]
    fn an_opted_out_participant_is_never_mixed_even_after_the_grace() {
        let mut taps = Taps::new(mixes());
        let bob = insert_tap(&mut taps, "TR_bob", Some("bob"));
        taps.set_opted_out("bob", true);
        let t0 = Instant::now();
        taps.tick(true, t0);
        taps.tick(true, t0 + REMOTE_AUDIO_GRACE * 10);
        assert!(!bob.load(Ordering::Relaxed));
    }

    #[test]
    fn someone_who_arrives_mid_stream_waits_their_own_grace() {
        let mut taps = Taps::new(mixes());
        let t0 = Instant::now();
        taps.tick(true, t0);
        let arrival = t0 + REMOTE_AUDIO_GRACE * 3;
        taps.consent.arrived("carol", arrival);
        let carol = insert_tap(&mut taps, "TR_carol", Some("carol"));
        taps.tick(true, arrival + Duration::from_secs(1));
        assert!(!carol.load(Ordering::Relaxed), "Carol hasn't had time to see LIVE");
        taps.tick(true, arrival + REMOTE_AUDIO_GRACE);
        assert!(carol.load(Ordering::Relaxed));
    }

    #[test]
    fn a_lost_announcement_takes_remote_audio_off_until_it_is_back_and_the_grace_passes() {
        let mut taps = Taps::new(mixes());
        let bob = insert_tap(&mut taps, "TR_bob", Some("bob"));
        let t0 = Instant::now();
        taps.tick(true, t0);
        taps.tick(true, t0 + REMOTE_AUDIO_GRACE);
        assert!(bob.load(Ordering::Relaxed));
        // A full reconnect dropped petal.live: off at once.
        let lost = t0 + REMOTE_AUDIO_GRACE * 2;
        taps.tick(false, lost);
        assert!(!bob.load(Ordering::Relaxed));
        // Rewritten: the grace starts again.
        taps.tick(true, lost + Duration::from_secs(1));
        taps.tick(true, lost + Duration::from_secs(2));
        assert!(!bob.load(Ordering::Relaxed));
        taps.tick(true, lost + Duration::from_secs(1) + REMOTE_AUDIO_GRACE);
        assert!(bob.load(Ordering::Relaxed));
    }

    #[test]
    fn a_reconnect_takes_remote_audio_off_until_the_announcement_is_back() {
        let mixes = mixes();
        let mut taps = announced_long_ago(mixes.clone());
        let bob = insert_tap(&mut taps, "TR_bob", Some("bob"));
        let t0 = Instant::now();
        taps.tick(true, t0);
        assert!(bob.load(Ordering::Relaxed));
        mixes.route("TR_bob", &block(1000), MIX_CHANNELS, &bob);
        // Reconnecting: off at once, queued audio dropped, even though the
        // keeper still reports the old announcement.
        taps.reconnecting = true;
        taps.tick(true, t0);
        assert!(!bob.load(Ordering::Relaxed));
        assert_eq!(mixes.stream.lock_unpoisoned().source_count(), 0);
        taps.tick(true, t0 + REMOTE_AUDIO_GRACE * 4);
        assert!(!bob.load(Ordering::Relaxed), "nothing while the room is reconnecting");
        // Back, and re-announced: the grace runs again.
        taps.reconnecting = false;
        let back = t0 + REMOTE_AUDIO_GRACE * 5;
        taps.tick(true, back);
        assert!(!bob.load(Ordering::Relaxed));
        taps.tick(true, back + REMOTE_AUDIO_GRACE);
        assert!(bob.load(Ordering::Relaxed));
    }

    #[test]
    fn an_opt_out_lost_to_a_rejoin_keeps_the_participant_out() {
        let mut taps = announced_long_ago(mixes());
        let bob = insert_tap(&mut taps, "TR_bob", Some("bob"));
        taps.observe_opt_out("bob", Some("1"));
        assert!(!bob.load(Ordering::Relaxed));
        // Bob's full reconnect: he reappears (and his mic republishes)
        // before his opt-out is written again. Absent is not an opt-in.
        taps.observe_opt_out("bob", None);
        taps.tick(true, Instant::now() + REMOTE_AUDIO_GRACE * 10);
        assert!(!bob.load(Ordering::Relaxed));
        // Only an explicit "0" brings him back.
        taps.observe_opt_out("bob", Some("0"));
        taps.reevaluate(Instant::now());
        assert!(bob.load(Ordering::Relaxed));
        // Someone never seen opted out is included while absent.
        let carol = insert_tap(&mut taps, "TR_carol", Some("carol"));
        taps.observe_opt_out("carol", None);
        taps.reevaluate(Instant::now());
        assert!(carol.load(Ordering::Relaxed));
    }

    #[test]
    fn a_detached_track_leaves_no_queued_audio_in_the_stream() {
        let mixes = mixes();
        let mut taps = announced_long_ago(mixes.clone());
        let bob = insert_tap(&mut taps, "TR_bob", Some("bob"));
        taps.reevaluate(Instant::now());
        mixes.route("TR_bob", &block(1000), MIX_CHANNELS, &bob);
        assert_eq!(mixes.stream.lock_unpoisoned().source_count(), 1);
        taps.detach("TR_bob");
        assert_eq!(mixes.stream.lock_unpoisoned().source_count(), 0);
        // A frame the reader task delivers after the detach goes nowhere.
        mixes.route("TR_bob", &block(1000), MIX_CHANNELS, &bob);
        assert_eq!(mixes.stream.lock_unpoisoned().source_count(), 0);
    }

    #[test]
    fn an_opted_out_participant_feeds_the_echo_reference_but_never_the_stream() {
        let mixes = mixes();
        let mut taps = announced_long_ago(mixes.clone());
        let in_stream = Arc::new(AtomicBool::new(true));
        taps.taps.insert(
            "TR_bob".into(),
            Tap { cancel: CancellationToken::new(), owner: Some("bob".into()), in_stream: in_stream.clone() },
        );
        // Included: both mixes hear Bob.
        mixes.route("TR_bob", &block(1000), MIX_CHANNELS, &in_stream);
        assert_eq!(mixes.stream.lock_unpoisoned().source_count(), 1);
        // Bob opts out: his queued audio leaves the stream at once, and new
        // frames only reach the echo reference.
        taps.set_opted_out("bob", true);
        assert_eq!(mixes.stream.lock_unpoisoned().source_count(), 0);
        mixes.route("TR_bob", &block(1000), MIX_CHANNELS, &in_stream);
        assert_eq!(mixes.stream.lock_unpoisoned().source_count(), 0);
        assert!(mixes.stream.lock_unpoisoned().pull_block().iter().all(|s| *s == 0));
        assert!(mixes.reference.lock_unpoisoned().pull(480).iter().all(|s| *s == 1000));
        // Back in: the stream hears him again.
        taps.observe_opt_out("bob", Some("0"));
        mixes.route("TR_bob", &block(1000), MIX_CHANNELS, &in_stream);
        assert!(mixes.stream.lock_unpoisoned().pull_block().iter().all(|s| *s == 1000));
    }

    #[test]
    fn only_the_opted_out_participants_tracks_leave_the_stream() {
        let mixes = mixes();
        let mut taps = announced_long_ago(mixes.clone());
        let flags: Vec<Arc<AtomicBool>> = (0..3).map(|_| Arc::new(AtomicBool::new(true))).collect();
        for (sid, owner, flag) in [
            ("TR_bob_mic", Some("bob"), &flags[0]),
            ("TR_bob_app", Some("bob"), &flags[1]),
            ("TR_ours", None, &flags[2]),
        ] {
            taps.taps.insert(
                sid.into(),
                Tap { cancel: CancellationToken::new(), owner: owner.map(Into::into), in_stream: flag.clone() },
            );
        }
        taps.set_opted_out("bob", true);
        assert!(!flags[0].load(Ordering::Relaxed) && !flags[1].load(Ordering::Relaxed));
        assert!(flags[2].load(Ordering::Relaxed), "our own audio is never opted out");
        // Someone who isn't in the room opting out changes nothing here.
        taps.set_opted_out("carol", true);
        assert!(flags[2].load(Ordering::Relaxed));
    }
}
