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

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use livekit::prelude::{LocalTrack, Participant, RemoteTrack, Room, RoomEvent, TrackSource};
use livekit::webrtc::audio_stream::native::NativeAudioStream;
use livekit::webrtc::prelude::RtcAudioTrack;
use tokio_util::sync::CancellationToken;

use super::mixer::{AudioMixer, MIX_CHANNELS, MIX_SAMPLE_RATE};
use super::wire::{is_opted_out, STREAM_OPTOUT_ATTRIBUTE};
use crate::sync_ext::MutexExt;

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
}

impl Taps {
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
        let include = owner.as_ref().is_none_or(|owner| !self.opted_out.contains(owner));
        log::info!(
            "streaming: stream audio + {label} ({sid}){}",
            if include { "" } else { " (kept off the stream)" }
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
            tap.cancel.cancel();
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
        for (sid, tap) in &self.taps {
            if tap.owner.as_deref() == Some(identity) {
                self.mixes.set_in_stream(sid, &tap.in_stream, !opted_out);
            }
        }
    }

    fn cancel_all(&mut self) {
        for (_, tap) in self.taps.drain() {
            tap.cancel.cancel();
        }
    }
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
pub fn start(
    room: Arc<Room>,
    mixer: Arc<Mutex<AudioMixer>>,
    reference: Arc<Mutex<AudioMixer>>,
    cancel: CancellationToken,
) {
    let mut taps = Taps {
        mixes: Mixes { stream: mixer, reference },
        taps: HashMap::new(),
        opted_out: HashSet::new(),
    };
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
        loop {
            let event = tokio::select! {
                () = cancel.cancelled() => break,
                event = events.recv() => match event { Some(event) => event, None => break },
            };
            match event {
                RoomEvent::TrackSubscribed { track, publication, participant } => {
                    if let Some(rtc) = remote_rtc(&track) {
                        let owner = participant.identity().to_string();
                        taps.attach(publication.sid().to_string(), Some(owner), rtc, &cancel);
                    }
                }
                RoomEvent::TrackUnsubscribed { publication, .. } => {
                    taps.detach(&publication.sid().to_string());
                }
                RoomEvent::ParticipantAttributesChanged { participant: Participant::Remote(participant), .. } => {
                    let opted_out =
                        is_opted_out(participant.attributes().get(STREAM_OPTOUT_ATTRIBUTE).map(String::as_str));
                    taps.set_opted_out(&participant.identity().to_string(), opted_out);
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

    #[test]
    fn an_opted_out_participant_feeds_the_echo_reference_but_never_the_stream() {
        let mixes = mixes();
        let mut taps = Taps { mixes: mixes.clone(), taps: HashMap::new(), opted_out: HashSet::new() };
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
        taps.set_opted_out("bob", false);
        mixes.route("TR_bob", &block(1000), MIX_CHANNELS, &in_stream);
        assert!(mixes.stream.lock_unpoisoned().pull_block().iter().all(|s| *s == 1000));
    }

    #[test]
    fn only_the_opted_out_participants_tracks_leave_the_stream() {
        let mixes = mixes();
        let mut taps = Taps { mixes: mixes.clone(), taps: HashMap::new(), opted_out: HashSet::new() };
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
