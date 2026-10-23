//! What the microphone actually captures during a Windows call.
//!
//! The device list cannot tell a live microphone from a dead one. A wireless
//! headset's USB dongle stays an `ACTIVE` endpoint while the headset is off
//! (see `windows_audio_device::active_endpoint_ids`), so a call keeps using
//! it while it may capture nothing. This reads what WebRTC reports the mic
//! captured and logs a line whenever the signal state changes, plus a
//! heartbeat, so a log shows when the signal stopped or resumed without
//! anyone noting times. [`DeadMicDetector`] turns the same signal into the
//! decision to ask the user whether to move capture to another microphone.

/// Cumulative capture counters from WebRTC's `media-source` stats for the mic.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CaptureCounters {
    /// Sum of (sample level squared x duration), levels in 0..=1.
    pub total_audio_energy: f64,
    /// Seconds of audio captured.
    pub total_samples_duration: f64,
}

/// Below about -100 dBFS: digital silence, which a live microphone's own
/// noise floor never reaches.
const SILENT_POWER: f64 = 1e-10;

/// One logged line every this many observations even when nothing changed
/// (the session watcher observes every 2s, so every 30s).
const HEARTBEAT_EVERY: u32 = 15;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Signal {
    /// No `media-source` stats for the mic.
    Unavailable,
    /// The capture produced no samples since the last observation.
    Stalled,
    /// Samples, all digital silence.
    Silent,
    /// Samples with a signal (at least the mic's noise floor).
    Live,
}

impl Signal {
    fn as_str(self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::Stalled => "stalled",
            Self::Silent => "silent",
            Self::Live => "live",
        }
    }
}

/// What one observation found, when it is worth a log line.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MicSignalReport {
    pub previous: Option<Signal>,
    pub signal: Signal,
    /// Average level over the window, when there were samples.
    pub level_dbfs: Option<f64>,
    pub captured_seconds: f64,
    pub muted: bool,
}

impl MicSignalReport {
    pub(crate) fn line(&self, device: &str) -> String {
        let change = match self.previous {
            Some(previous) if previous != self.signal => {
                format!("{} -> {}", previous.as_str(), self.signal.as_str())
            }
            Some(_) => format!("{} (unchanged)", self.signal.as_str()),
            None => self.signal.as_str().to_string(),
        };
        let level = self
            .level_dbfs
            .map_or_else(|| "n/a".to_string(), |dbfs| format!("{dbfs:.1} dBFS"));
        format!(
            "audio: mic signal {change} -- device={device} muted={} level={level} captured={:.1}s",
            self.muted, self.captured_seconds
        )
    }
}

/// The mic's capture counters from WebRTC's `media-source` stats. `None` when
/// the stats are unavailable, including when they take longer than a second.
pub(crate) async fn capture_counters(
    track: &livekit::prelude::LocalAudioTrack,
) -> Option<CaptureCounters> {
    use livekit::webrtc::stats::RtcStats;
    let stats = tokio::time::timeout(std::time::Duration::from_secs(1), track.get_stats())
        .await
        .ok()?
        .ok()?;
    stats.iter().find_map(|stat| match stat {
        RtcStats::MediaSource(source) if source.source.kind == "audio" => Some(CaptureCounters {
            total_audio_energy: source.audio.total_audio_energy,
            total_samples_duration: source.audio.total_samples_duration,
        }),
        _ => None,
    })
}

/// Pure state for the mic signal log; fed one observation per watcher tick.
#[derive(Default)]
pub(crate) struct MicSignalProbe {
    last_counters: Option<CaptureCounters>,
    /// Device, mute state and signal of the last logged line.
    last_logged: Option<(String, bool, Signal)>,
    since_logged: u32,
    /// The latest observation's signal; `None` after a baseline-only one.
    last_signal: Option<Signal>,
}

impl MicSignalProbe {
    /// Returns a report when the device, mute state or signal changed since
    /// the last logged line, or when a heartbeat is due. The first observation
    /// only records a baseline, because levels are differences of counters.
    pub(crate) fn observe(
        &mut self,
        device_id: &str,
        muted: bool,
        counters: Option<CaptureCounters>,
    ) -> Option<MicSignalReport> {
        let (signal, level_dbfs, captured_seconds) = match (self.last_counters, counters) {
            (_, None) => (Signal::Unavailable, None, 0.0),
            (Some(last), Some(now))
                if now.total_samples_duration >= last.total_samples_duration
                    && now.total_audio_energy >= last.total_audio_energy =>
            {
                let seconds = now.total_samples_duration - last.total_samples_duration;
                if seconds <= 0.0 {
                    (Signal::Stalled, None, 0.0)
                } else {
                    let power = (now.total_audio_energy - last.total_audio_energy) / seconds;
                    if power <= SILENT_POWER {
                        (Signal::Silent, None, seconds)
                    } else {
                        (Signal::Live, Some(10.0 * power.log10()), seconds)
                    }
                }
            }
            // First observation, or counters that went backwards (a new
            // source): only a baseline.
            (_, Some(_)) => {
                self.last_counters = counters;
                self.last_signal = None;
                return None;
            }
        };
        self.last_counters = counters;
        self.last_signal = Some(signal);
        self.since_logged += 1;

        let previous = self.last_logged.as_ref().map(|(_, _, signal)| *signal);
        let changed = self
            .last_logged
            .as_ref()
            .is_none_or(|(device, was_muted, was)| {
                device != device_id || *was_muted != muted || *was != signal
            });
        if !changed && self.since_logged < HEARTBEAT_EVERY {
            return None;
        }
        self.last_logged = Some((device_id.to_string(), muted, signal));
        self.since_logged = 0;
        Some(MicSignalReport {
            previous,
            signal,
            level_dbfs,
            captured_seconds,
            muted,
        })
    }

    pub(crate) fn last_signal(&self) -> Option<Signal> {
        self.last_signal
    }
}

/// Silent observations in a row (about 6s) before the mic counts as dead. A
/// live mic's noise floor never reads as digital silence, whether or not Petal
/// has it muted.
const DEAD_AFTER: u32 = 3;

/// Pure: decides when to ask whether the microphone capture is on has gone
/// dead.
///
/// Digital silence is not proof on its own. A mic muted at the Windows
/// endpoint (a laptop's mic-mute key, Sound settings, a headset's mute
/// button) captures exact silence, and so do noise-gated headsets and virtual
/// noise-removal mics between words. So silence from a muted endpoint never
/// counts, the user is asked rather than switched, and each device is asked
/// about at most once per call.
#[derive(Default)]
pub(crate) struct DeadMicDetector {
    device: String,
    silent: u32,
    asked: std::collections::HashSet<String>,
}

impl DeadMicDetector {
    /// True once, after `DEAD_AFTER` silent observations in a row of the same
    /// device, unless the user was already asked about it. A silent
    /// observation from an endpoint Windows has muted, any other signal, or a
    /// device change starts the count again; a baseline-only observation
    /// (`None`) neither counts nor resets. `endpoint_muted` is only called for
    /// a silent observation.
    pub(crate) fn observe(
        &mut self,
        device_id: &str,
        signal: Option<Signal>,
        endpoint_muted: impl FnOnce() -> bool,
    ) -> bool {
        if self.device != device_id {
            self.device = device_id.to_owned();
            self.silent = 0;
        }
        match signal {
            Some(Signal::Silent) if !device_id.is_empty() && !endpoint_muted() => {
                self.silent += 1;
                self.silent == DEAD_AFTER && !self.asked.contains(device_id)
            }
            None => false,
            Some(_) => {
                self.silent = 0;
                false
            }
        }
    }

    /// The user was asked about `device_id`: don't ask again this call.
    pub(crate) fn asked(&mut self, device_id: &str) {
        self.asked.insert(device_id.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counters(energy: f64, seconds: f64) -> Option<CaptureCounters> {
        Some(CaptureCounters {
            total_audio_energy: energy,
            total_samples_duration: seconds,
        })
    }

    #[test]
    fn a_dead_capture_then_a_live_one_is_logged_at_each_change() {
        let mut probe = MicSignalProbe::default();
        assert_eq!(
            probe.observe("corsair", false, counters(0.0, 0.0)),
            None,
            "baseline"
        );

        // Headset off: two seconds of digital silence.
        let silent = probe.observe("corsair", false, counters(0.0, 2.0)).unwrap();
        assert_eq!((silent.previous, silent.signal), (None, Signal::Silent));
        assert_eq!(silent.level_dbfs, None);
        assert_eq!(
            probe.observe("corsair", false, counters(0.0, 4.0)),
            None,
            "unchanged"
        );

        // Headset on: a noise floor around -60 dBFS (power 1e-6 over 2s).
        let live = probe
            .observe("corsair", false, counters(2e-6, 6.0))
            .unwrap();
        assert_eq!(
            (live.previous, live.signal),
            (Some(Signal::Silent), Signal::Live)
        );
        assert!((live.level_dbfs.unwrap() + 60.0).abs() < 1e-9);
        assert_eq!(
            live.line("'Headset Microphone (CORSAIR)'"),
            "audio: mic signal silent -> live -- device='Headset Microphone (CORSAIR)' muted=false level=-60.0 dBFS captured=2.0s"
        );
    }

    #[test]
    fn stalled_unavailable_and_mute_changes_are_logged() {
        let mut probe = MicSignalProbe::default();
        probe.observe("mic", false, counters(0.0, 0.0));
        assert_eq!(
            probe
                .observe("mic", false, counters(1e-6, 2.0))
                .unwrap()
                .signal,
            Signal::Live
        );
        // No new samples: the capture thread ended.
        assert_eq!(
            probe
                .observe("mic", false, counters(1e-6, 2.0))
                .unwrap()
                .signal,
            Signal::Stalled
        );
        assert!(
            probe
                .observe("mic", true, counters(1e-6, 2.0))
                .unwrap()
                .muted,
            "mute change"
        );
        assert_eq!(
            probe.observe("mic", true, None).unwrap().signal,
            Signal::Unavailable
        );
    }

    #[test]
    fn a_device_switch_is_logged_and_counters_going_backwards_only_rebaseline() {
        let mut probe = MicSignalProbe::default();
        probe.observe("corsair", false, counters(0.0, 0.0));
        probe.observe("corsair", false, counters(1e-6, 2.0));
        let switched = probe.observe("webcam", false, counters(2e-6, 4.0)).unwrap();
        assert_eq!(switched.signal, Signal::Live);
        assert_eq!(
            probe.observe("webcam", false, counters(0.0, 0.5)),
            None,
            "rebaseline"
        );
        assert_eq!(
            probe.observe("webcam", false, counters(1e-6, 2.5)),
            None,
            "still live"
        );
    }

    #[test]
    fn an_unchanged_signal_still_logs_a_heartbeat_every_30_seconds() {
        let mut probe = MicSignalProbe::default();
        probe.observe("mic", false, counters(0.0, 0.0));
        assert!(probe.observe("mic", false, counters(1e-6, 2.0)).is_some());
        let mut logged = Vec::new();
        for tick in 2..=31 {
            let seconds = f64::from(tick) * 2.0;
            logged.push(
                probe
                    .observe("mic", false, counters(seconds * 5e-7, seconds))
                    .is_some(),
            );
        }
        assert_eq!(logged.iter().filter(|line| **line).count(), 2);
        assert!(logged[14] && logged[29], "every 15th observation");
    }

    const SILENT: Option<Signal> = Some(Signal::Silent);
    const UNMUTED: fn() -> bool = || false;

    #[test]
    fn a_mic_is_dead_after_three_silent_observations_and_fires_once() {
        let mut detector = DeadMicDetector::default();
        assert!(!detector.observe("corsair", None, UNMUTED), "baseline");
        assert!(!detector.observe("corsair", SILENT, UNMUTED));
        assert!(!detector.observe("corsair", SILENT, UNMUTED));
        assert!(
            detector.observe("corsair", SILENT, UNMUTED),
            "about 6s of silence"
        );
        assert!(!detector.observe("corsair", SILENT, UNMUTED), "only once");
        assert!(!detector.observe("corsair", SILENT, UNMUTED));
    }

    #[test]
    fn live_or_stalled_signal_and_device_changes_restart_the_count() {
        let mut detector = DeadMicDetector::default();
        detector.observe("corsair", SILENT, UNMUTED);
        detector.observe("corsair", SILENT, UNMUTED);
        assert!(
            !detector.observe("corsair", Some(Signal::Live), UNMUTED),
            "speech resets"
        );
        assert!(!detector.observe("corsair", SILENT, UNMUTED));
        assert!(!detector.observe("corsair", Some(Signal::Stalled), UNMUTED));
        assert!(!detector.observe("corsair", SILENT, UNMUTED));
        assert!(!detector.observe("corsair", SILENT, UNMUTED));
        assert!(
            !detector.observe("webcam", SILENT, UNMUTED),
            "new device, new count"
        );
        assert!(!detector.observe("webcam", SILENT, UNMUTED));
        assert!(detector.observe("webcam", SILENT, UNMUTED));
        // No known device: never declared dead.
        let mut unknown = DeadMicDetector::default();
        for _ in 0..5 {
            assert!(!unknown.observe("", SILENT, UNMUTED));
        }
    }

    #[test]
    fn silence_from_an_endpoint_windows_has_muted_never_counts() {
        let mut detector = DeadMicDetector::default();
        detector.observe("laptop", SILENT, UNMUTED);
        detector.observe("laptop", SILENT, UNMUTED);
        // The mic-mute key: the endpoint goes silent because it is muted.
        for _ in 0..10 {
            assert!(
                !detector.observe("laptop", SILENT, || true),
                "muted at the endpoint"
            );
        }
        // Unmuted again and still silent: a fresh count.
        assert!(!detector.observe("laptop", SILENT, UNMUTED));
        assert!(!detector.observe("laptop", SILENT, UNMUTED));
        assert!(detector.observe("laptop", SILENT, UNMUTED));
    }

    #[test]
    fn the_endpoint_mute_is_only_read_for_a_silent_observation() {
        let mut detector = DeadMicDetector::default();
        let unread = || -> bool { panic!("endpoint mute read for a non-silent observation") };
        detector.observe("corsair", None, unread);
        detector.observe("corsair", Some(Signal::Live), unread);
        detector.observe("corsair", Some(Signal::Stalled), unread);
        detector.observe("corsair", Some(Signal::Unavailable), unread);
    }

    #[test]
    fn a_device_is_asked_about_at_most_once_per_call() {
        // A noise-gated headset: silent at every pause, live between them.
        let mut detector = DeadMicDetector::default();
        let pause = |detector: &mut DeadMicDetector| {
            detector.observe("gated", Some(Signal::Live), UNMUTED);
            (0..DEAD_AFTER).any(|_| detector.observe("gated", SILENT, UNMUTED))
        };
        assert!(pause(&mut detector), "the first long pause asks");
        detector.asked("gated");
        for _ in 0..5 {
            assert!(!pause(&mut detector), "later pauses do not ask again");
        }
        // Not asked (no other mic to offer): the next long pause may ask.
        let mut detector = DeadMicDetector::default();
        assert!(pause(&mut detector));
        assert!(pause(&mut detector));
    }
}
