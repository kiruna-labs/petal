//! #411: detect a VideoToolbox H.264 encoder that has stopped producing frames,
//! and decide when to re-create it.
//!
//! ## Why this exists
//!
//! libwebrtc's `RTCVideoEncoderH264` (the pinned prebuilt is webrtc-sdk
//! `m144_release` @ `ec20be4f94`; its `RTCVideoEncoderH264.mm:899` /`:450` /
//! `:614` log sites match the field logs line for line) reports an
//! ASYNCHRONOUS encode failure -- a non-`noErr` status in VideoToolbox's output
//! callback -- with exactly one `LS_ERROR` line, "H264 encode failed with code:
//! <status>", and then returns. It never resets the compression session for
//! it: only a SYNCHRONOUS `kVTInvalidSessionErr` / `kVTVideoEncoderMalfunctionErr`
//! from `VTCompressionSessionEncodeFrame` does. So a session that starts failing
//! keeps failing for as long as the track lives. On macOS 26 that was ~58,000
//! failures over a 37-minute meeting at a steady 15-20/s, with the camera track
//! published but carrying nothing.
//!
//! Nothing above libwebrtc saw it. `camera publish health ... encode_fps` counts
//! frames Petal PUSHED into the source, not frames the encoder EMITTED, and the
//! libwebrtc target is breadcrumb-only for Sentry (`logging::sentry_log_filter`),
//! so not one Sentry issue exists for the signature.
//!
//! ## What it does
//!
//! Two independent signals, because neither is enough alone:
//!
//! - [`note_native_webrtc_message`] counts every encode-failure line, by
//!   VideoToolbox status class, from the raw libwebrtc log stream (before the
//!   #108 per-site repeat suppression collapses them). The line names no track,
//!   so it can only say "some VideoToolbox encoder in this process is failing".
//! - The per-track [`EncoderStarvationDetector`] compares frames pushed into a
//!   track with that track's own `outbound-rtp` `frames_encoded`. That is what
//!   attributes a failure streak to ONE track: a healthy share keeps encoding
//!   while the camera beside it starves.
//!
//! A sample is FAILING only when both hold: the track encoded almost none of
//! what it was fed AND the process logged VideoToolbox encode failures in the
//! same interval. Starvation without VideoToolbox errors is something else
//! (bandwidth, the frame dropper, a paused sender) and is left alone.
//!
//! After [`FAILING_SAMPLES_BEFORE_RECREATE`] consecutive failing samples the
//! detector asks for the encoder to be re-created (the caller republishes the
//! track, which builds a new sender, a new `VideoStreamEncoder` and a new
//! `VTCompressionSession`). A flap guard bounds that: more than
//! [`MAX_RECREATES_PER_WINDOW`] re-creations inside [`RECREATE_WINDOW`] means
//! re-creating does not help, and the detector gives up for the track's
//! lifetime rather than republishing in a loop.

// The recovery loop that consumes the detector is macOS-only (VideoToolbox);
// the counter and classifier still compile everywhere so the tests run on
// every host.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// How often a caller should sample a track (frames pushed / frames encoded).
pub(crate) const ENCODER_HEALTH_SAMPLE_INTERVAL: Duration = Duration::from_secs(2);

/// Consecutive failing samples before the encoder is re-created. At the 2 s
/// sample interval this is ~6 s of a track carrying nothing.
pub(crate) const FAILING_SAMPLES_BEFORE_RECREATE: u32 = 3;

/// Samples ignored right after a re-creation: the republished track has to
/// negotiate and its new encoder has to start, so its first interval can
/// legitimately encode little.
pub(crate) const POST_RECREATE_GRACE_SAMPLES: u32 = 2;

/// Below this many pushed frames in one sample the source is not feeding the
/// encoder enough to judge it (camera paused, capture hiccup). Such a sample
/// neither advances nor clears a streak.
pub(crate) const MIN_PUSHED_FRAMES_PER_SAMPLE: u64 = 10;

/// A sample is "starved" when fewer than 1/STARVED_RATIO_DENOMINATOR of the
/// pushed frames came out encoded. 25%: libwebrtc's own level clamp (13 of 30
/// fps at 1080p on Level 3.1) and ordinary frame dropping stay well above it.
pub(crate) const STARVED_RATIO_DENOMINATOR: u64 = 4;

/// Flap guard: at most this many re-creations inside [`RECREATE_WINDOW`].
pub(crate) const MAX_RECREATES_PER_WINDOW: u32 = 3;
pub(crate) const RECREATE_WINDOW: Duration = Duration::from_secs(10 * 60);

/// VideoToolbox status classes (`VTErrors.h`). Closed, so telemetry can carry
/// one without carrying a raw number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum VtFailureClass {
    /// -12902 `kVTParameterErr`.
    Parameter,
    /// -12903 `kVTInvalidSessionErr`.
    InvalidSession,
    /// -12911. `VTErrors.h` names this `kVTVideoDecoderMalfunctionErr`, but the
    /// field logs show the ENCODER's output callback returning it (#411), so it
    /// is classified on its own rather than folded into either malfunction.
    MalfunctionDecoderCode,
    /// -12912 `kVTVideoEncoderMalfunctionErr`.
    EncoderMalfunction,
    /// -12915 `kVTVideoEncoderNotAvailableNowErr` (#427's wedged-encoder state).
    NotAvailableNow,
    /// Any other status.
    Other,
}

impl VtFailureClass {
    pub(crate) const ALL: [VtFailureClass; 6] = [
        VtFailureClass::Parameter,
        VtFailureClass::InvalidSession,
        VtFailureClass::MalfunctionDecoderCode,
        VtFailureClass::EncoderMalfunction,
        VtFailureClass::NotAvailableNow,
        VtFailureClass::Other,
    ];

    pub(crate) fn from_status(status: i64) -> Self {
        match status {
            -12902 => Self::Parameter,
            -12903 => Self::InvalidSession,
            -12911 => Self::MalfunctionDecoderCode,
            -12912 => Self::EncoderMalfunction,
            -12915 => Self::NotAvailableNow,
            _ => Self::Other,
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Parameter => 0,
            Self::InvalidSession => 1,
            Self::MalfunctionDecoderCode => 2,
            Self::EncoderMalfunction => 3,
            Self::NotAvailableNow => 4,
            Self::Other => 5,
        }
    }

    pub(crate) fn tag(self) -> &'static str {
        match self {
            Self::Parameter => "parameter",
            Self::InvalidSession => "invalid_session",
            Self::MalfunctionDecoderCode => "malfunction_12911",
            Self::EncoderMalfunction => "encoder_malfunction",
            Self::NotAvailableNow => "not_available_now",
            Self::Other => "other",
        }
    }
}

/// Per-class encode-failure counts, as a monotonic snapshot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct VtFailureCounts([u64; 6]);

impl VtFailureCounts {
    pub(crate) fn total(&self) -> u64 {
        self.0.iter().fold(0u64, |sum, value| sum.saturating_add(*value))
    }

    pub(crate) fn get(&self, class: VtFailureClass) -> u64 {
        self.0[class.index()]
    }

    /// `self - earlier`, per class, saturating (a counter never runs backwards,
    /// but a test or a reset must not wrap).
    pub(crate) fn since(&self, earlier: &VtFailureCounts) -> VtFailureCounts {
        let mut delta = [0u64; 6];
        for (index, value) in delta.iter_mut().enumerate() {
            *value = self.0[index].saturating_sub(earlier.0[index]);
        }
        VtFailureCounts(delta)
    }

    fn add(&mut self, other: &VtFailureCounts) {
        for (index, value) in self.0.iter_mut().enumerate() {
            *value = value.saturating_add(other.0[index]);
        }
    }

    /// The class with the most failures; `None` when there are none. Ties go to
    /// the earlier class in [`VtFailureClass::ALL`], so the answer is stable.
    pub(crate) fn dominant(&self) -> Option<VtFailureClass> {
        let mut best: Option<(VtFailureClass, u64)> = None;
        for class in VtFailureClass::ALL {
            let count = self.get(class);
            if count > 0 && best.is_none_or(|(_, best_count)| count > best_count) {
                best = Some((class, count));
            }
        }
        best.map(|(class, _)| class)
    }

    #[cfg(test)]
    pub(crate) fn of(class: VtFailureClass, count: u64) -> Self {
        let mut counts = Self::default();
        counts.0[class.index()] = count;
        counts
    }
}

static VT_ENCODE_FAILURES: [AtomicU64; 6] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// Process-wide encode-failure counts so far.
pub(crate) fn vt_encode_failure_counts() -> VtFailureCounts {
    let mut counts = [0u64; 6];
    for (index, value) in counts.iter_mut().enumerate() {
        *value = VT_ENCODE_FAILURES[index].load(Ordering::Relaxed);
    }
    VtFailureCounts(counts)
}

/// The libwebrtc lines that mean "a VideoToolbox H.264 encode did not produce a
/// frame" and carry a status: the literal each one prints right before it.
/// The literals must track `RTCVideoEncoderH264.mm` (the #866 lesson: a matcher
/// pointed at a line that stops existing silently matches nothing); the unit
/// tests pin them against verbatim field lines.
const ENCODE_FAILURE_PREFIXES: [&str; 2] = [
    // Async: VideoToolbox's output callback reported a failure (#411).
    "H264 encode failed with code: ",
    // Sync: `VTCompressionSessionEncodeFrame` itself failed.
    "Failed to encode frame with code: ",
];
/// Sync `kVTVideoEncoderMalfunctionErr`: libwebrtc resets the session for this
/// one itself, but it is still a frame that was not encoded.
const SYNC_MALFUNCTION_LINE: &str = "Encountered video encoder malfunction error";

/// Classify one raw libwebrtc log message. `None` for anything that is not an
/// encode failure.
pub(crate) fn classify_encode_failure_line(message: &str) -> Option<VtFailureClass> {
    if !message.contains("RTCVideoEncoderH264") && !message.contains("H264 encode failed") {
        return None;
    }
    for prefix in ENCODE_FAILURE_PREFIXES {
        if let Some(position) = message.find(prefix) {
            let rest = &message[position + prefix.len()..];
            let end = rest
                .char_indices()
                .find(|(index, c)| !(c.is_ascii_digit() || (*index == 0 && *c == '-')))
                .map_or(rest.len(), |(index, _)| index);
            return Some(
                rest[..end]
                    .parse::<i64>()
                    .map_or(VtFailureClass::Other, VtFailureClass::from_status),
            );
        }
    }
    message
        .contains(SYNC_MALFUNCTION_LINE)
        .then_some(VtFailureClass::EncoderMalfunction)
}

/// Hook for the raw native-WebRTC log stream (`logging::RepeatSuppressingLog`
/// calls this for EVERY record, before repeat suppression). Cheap on the
/// non-matching path: one or two substring scans.
pub(crate) fn note_native_webrtc_message(message: &str) {
    if let Some(class) = classify_encode_failure_line(message) {
        VT_ENCODE_FAILURES[class.index()].fetch_add(1, Ordering::Relaxed);
    }
}

/// One observation of a track: monotonic totals, read at `at`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct EncoderHealthSample {
    /// Frames Petal pushed into the track's source (monotonic).
    pub pushed_frames: u64,
    /// The track's `outbound-rtp` `frames_encoded`, summed over its encodings.
    /// `None` when stats could not be read this time. Restarts at 0 after a
    /// republish (new sender), which the detector treats as a re-baseline.
    pub encoded_frames: Option<u64>,
    /// Process-wide VideoToolbox encode-failure counts (monotonic).
    pub vt_failures: VtFailureCounts,
    pub at: Instant,
}

/// What the caller should do after a sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EncoderHealthAction {
    /// Nothing to do.
    None,
    /// Re-create the encoder (republish the track). `attempt` is 1-based within
    /// the flap window; `vt_failures` is what the streak saw.
    Recreate {
        attempt: u32,
        vt_failures: u64,
        dominant: VtFailureClass,
    },
    /// The first healthy sample after one or more re-creations.
    Recovered { attempts: u32 },
    /// Re-creating did not help; stop trying for this track's lifetime.
    GiveUp {
        attempts: u32,
        vt_failures: u64,
        dominant: VtFailureClass,
    },
}

/// Per-interval verdict, exposed for the periodic log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SampleVerdict {
    /// First sample, a stats gap, or the post-recreate grace.
    Baseline,
    /// The source pushed too little to judge.
    Idle,
    Healthy,
    /// Starved, but the process logged no VideoToolbox failures: not ours.
    StarvedWithoutVtFailures,
    Failing,
}

#[derive(Debug, Clone, Copy)]
struct Previous {
    pushed_frames: u64,
    encoded_frames: u64,
    vt_failures: VtFailureCounts,
}

/// The per-track state machine. Pure: no clock, no I/O -- the caller passes
/// every sample in, which is what lets the unit tests drive the real thing.
#[derive(Debug, Default)]
pub(crate) struct EncoderStarvationDetector {
    previous: Option<Previous>,
    failing_streak: u32,
    streak_failures: VtFailureCounts,
    grace_remaining: u32,
    /// Re-creations not yet followed by a healthy sample.
    unresolved_recreates: u32,
    recreate_times: Vec<Instant>,
    gave_up: bool,
    last_verdict: Option<SampleVerdict>,
    last_interval: Option<(u64, u64, u64)>,
}

impl EncoderStarvationDetector {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub(crate) fn gave_up(&self) -> bool {
        self.gave_up
    }

    pub(crate) fn last_verdict(&self) -> Option<SampleVerdict> {
        self.last_verdict
    }

    /// (pushed, encoded, vt_failures) deltas of the last judged interval.
    pub(crate) fn last_interval(&self) -> Option<(u64, u64, u64)> {
        self.last_interval
    }

    pub(crate) fn failing_streak(&self) -> u32 {
        self.failing_streak
    }

    /// The caller re-created the encoder (after a `Recreate` action). Starts the
    /// grace and forgets the old sender's counters.
    pub(crate) fn note_recreated(&mut self) {
        self.previous = None;
        self.grace_remaining = POST_RECREATE_GRACE_SAMPLES;
        self.failing_streak = 0;
        self.streak_failures = VtFailureCounts::default();
    }

    /// The caller tried to re-create the encoder and could not (republish
    /// failed). Counts toward the flap guard -- a republish that keeps failing
    /// must not be retried forever either -- but starts no grace.
    pub(crate) fn note_recreate_failed(&mut self) {
        self.previous = None;
        self.failing_streak = 0;
        self.streak_failures = VtFailureCounts::default();
    }

    pub(crate) fn observe(&mut self, sample: EncoderHealthSample) -> EncoderHealthAction {
        if self.gave_up {
            self.last_verdict = Some(SampleVerdict::Baseline);
            return EncoderHealthAction::None;
        }
        let Some(encoded_frames) = sample.encoded_frames else {
            // A stats gap: re-baseline on the next good read instead of
            // judging an interval whose start we never saw.
            self.previous = None;
            self.last_verdict = Some(SampleVerdict::Baseline);
            return EncoderHealthAction::None;
        };
        let current = Previous {
            pushed_frames: sample.pushed_frames,
            encoded_frames,
            vt_failures: sample.vt_failures,
        };
        let Some(previous) = self.previous.replace(current) else {
            self.last_verdict = Some(SampleVerdict::Baseline);
            return EncoderHealthAction::None;
        };
        if encoded_frames < previous.encoded_frames {
            // A new sender restarted its counters: re-baseline.
            self.last_verdict = Some(SampleVerdict::Baseline);
            return EncoderHealthAction::None;
        }
        if self.grace_remaining > 0 {
            self.grace_remaining -= 1;
            self.last_verdict = Some(SampleVerdict::Baseline);
            return EncoderHealthAction::None;
        }

        let pushed = sample.pushed_frames.saturating_sub(previous.pushed_frames);
        let encoded = encoded_frames - previous.encoded_frames;
        let failures = sample.vt_failures.since(&previous.vt_failures);
        self.last_interval = Some((pushed, encoded, failures.total()));

        if pushed < MIN_PUSHED_FRAMES_PER_SAMPLE {
            self.last_verdict = Some(SampleVerdict::Idle);
            return EncoderHealthAction::None;
        }
        let starved = encoded.saturating_mul(STARVED_RATIO_DENOMINATOR) < pushed;
        if !starved {
            self.last_verdict = Some(SampleVerdict::Healthy);
            self.failing_streak = 0;
            self.streak_failures = VtFailureCounts::default();
            if self.unresolved_recreates > 0 {
                let attempts = self.unresolved_recreates;
                self.unresolved_recreates = 0;
                return EncoderHealthAction::Recovered { attempts };
            }
            return EncoderHealthAction::None;
        }
        if failures.total() == 0 {
            self.last_verdict = Some(SampleVerdict::StarvedWithoutVtFailures);
            self.failing_streak = 0;
            self.streak_failures = VtFailureCounts::default();
            return EncoderHealthAction::None;
        }

        self.last_verdict = Some(SampleVerdict::Failing);
        self.failing_streak += 1;
        self.streak_failures.add(&failures);
        if self.failing_streak < FAILING_SAMPLES_BEFORE_RECREATE {
            return EncoderHealthAction::None;
        }

        let vt_failures = self.streak_failures.total();
        let dominant = self
            .streak_failures
            .dominant()
            .unwrap_or(VtFailureClass::Other);
        self.failing_streak = 0;
        self.streak_failures = VtFailureCounts::default();
        self.recreate_times
            .retain(|at| sample.at.saturating_duration_since(*at) < RECREATE_WINDOW);
        if self.recreate_times.len() as u32 >= MAX_RECREATES_PER_WINDOW {
            self.gave_up = true;
            return EncoderHealthAction::GiveUp {
                attempts: self.recreate_times.len() as u32,
                vt_failures,
                dominant,
            };
        }
        self.recreate_times.push(sample.at);
        self.unresolved_recreates += 1;
        EncoderHealthAction::Recreate {
            attempt: self.recreate_times.len() as u32,
            vt_failures,
            dominant,
        }
    }
}

/// Coarse bucket for a failure count, for telemetry (never a raw number).
pub(crate) fn failure_count_bucket(count: u64) -> &'static str {
    match count {
        0 => "0",
        1..=99 => "1_99",
        100..=999 => "100_999",
        _ => "1000_plus",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drives the detector on a synthetic clock with cumulative counters, the
    /// way the camera monitor feeds it.
    struct Feed {
        detector: EncoderStarvationDetector,
        t0: Instant,
        step: u32,
        pushed: u64,
        encoded: u64,
        failures: VtFailureCounts,
    }

    impl Feed {
        fn new() -> Self {
            let mut feed = Self {
                detector: EncoderStarvationDetector::new(),
                t0: Instant::now(),
                step: 0,
                pushed: 0,
                encoded: 0,
                failures: VtFailureCounts::default(),
            };
            // The baseline sample.
            assert_eq!(feed.sample(), EncoderHealthAction::None);
            feed
        }

        fn sample(&mut self) -> EncoderHealthAction {
            let at = self.t0 + ENCODER_HEALTH_SAMPLE_INTERVAL * self.step;
            self.step += 1;
            self.detector.observe(EncoderHealthSample {
                pushed_frames: self.pushed,
                encoded_frames: Some(self.encoded),
                vt_failures: self.failures,
                at,
            })
        }

        /// One 2 s interval at 30 fps pushed.
        fn interval(&mut self, encoded: u64, failures: VtFailureCounts) -> EncoderHealthAction {
            self.pushed += 60;
            self.encoded += encoded;
            self.failures.add(&failures);
            self.sample()
        }

        fn failing(&mut self) -> EncoderHealthAction {
            self.interval(0, VtFailureCounts::of(VtFailureClass::Parameter, 30))
        }

        fn healthy(&mut self) -> EncoderHealthAction {
            self.interval(58, VtFailureCounts::default())
        }

        /// A republish: the new sender's stats restart at 0.
        fn recreated(&mut self) {
            self.detector.note_recreated();
            self.encoded = 0;
        }

        /// Feed failing intervals until the detector acts (bounded).
        fn fail_until_action(&mut self) -> EncoderHealthAction {
            for _ in 0..20 {
                let action = self.failing();
                if action != EncoderHealthAction::None {
                    return action;
                }
            }
            panic!("no action after 20 failing samples");
        }

        /// Jump the synthetic clock forward without sampling.
        fn advance(&mut self, by: Duration) {
            let steps = (by.as_millis() / ENCODER_HEALTH_SAMPLE_INTERVAL.as_millis()) as u32;
            self.step += steps;
        }
    }

    #[test]
    fn classifies_verbatim_field_lines() {
        // Verbatim from the 2026-10-22 macOS 26.5.2 field log (#411).
        assert_eq!(
            classify_encode_failure_line(
                "(RTCVideoEncoderH264.mm:899): H264 encode failed with code: -12902"
            ),
            Some(VtFailureClass::Parameter)
        );
        assert_eq!(
            classify_encode_failure_line(
                "(RTCVideoEncoderH264.mm:899): H264 encode failed with code: -12911"
            ),
            Some(VtFailureClass::MalfunctionDecoderCode)
        );
        // #427's wedged-encoder codes.
        assert_eq!(
            classify_encode_failure_line(
                "(RTCVideoEncoderH264.mm:899): H264 encode failed with code: -12912"
            ),
            Some(VtFailureClass::EncoderMalfunction)
        );
        assert_eq!(
            classify_encode_failure_line(
                "(RTCVideoEncoderH264.mm:899): H264 encode failed with code: -12915"
            ),
            Some(VtFailureClass::NotAvailableNow)
        );
        // The synchronous paths.
        assert_eq!(
            classify_encode_failure_line(
                "(RTCVideoEncoderH264.mm:603): Failed to encode frame with code: -12903"
            ),
            Some(VtFailureClass::InvalidSession)
        );
        assert_eq!(
            classify_encode_failure_line(
                "(RTCVideoEncoderH264.mm:590): Encountered video encoder malfunction error. Resetting compression session."
            ),
            Some(VtFailureClass::EncoderMalfunction)
        );
        assert_eq!(
            classify_encode_failure_line(
                "(RTCVideoEncoderH264.mm:899): H264 encode failed with code: -1"
            ),
            Some(VtFailureClass::Other)
        );
    }

    #[test]
    fn ignores_the_neighbouring_encoder_lines() {
        for line in [
            // The level clamp warnings (#410) and the benign -12900 property
            // refusal fire alongside the failures but are not failures.
            "(RTCVideoEncoderH264.mm:614): Encoder frame rate setting 30 is larger than the maximal allowed frame rate 13.",
            "(RTCVideoEncoderH264.mm:450): Initial encoder frame rate setting 30 is larger than the maximal allowed frame rate 13.",
            "(helpers.cc:74): VTSessionSetProperty failed to set: PrioritizeEncodingSpeedOverQuality to 1: -12900",
            // A DECODE failure is the receiver's problem, not this one.
            "(RTCVideoDecoderH264.mm:101): Failed to decode frame. Status: -12909",
            "(video_encoder_software_fallback_wrapper.cc:184): Trying to access encoder in uninitialized fallback wrapper.",
        ] {
            assert_eq!(classify_encode_failure_line(line), None, "{line}");
        }
    }

    #[test]
    fn the_hook_counts_by_class() {
        let before = vt_encode_failure_counts();
        note_native_webrtc_message("(RTCVideoEncoderH264.mm:899): H264 encode failed with code: -12902");
        note_native_webrtc_message("(RTCVideoEncoderH264.mm:899): H264 encode failed with code: -12902");
        note_native_webrtc_message("(RTCVideoEncoderH264.mm:899): H264 encode failed with code: -12915");
        note_native_webrtc_message("(pacing_controller.cc:301): Elapsed time longer than expected");
        let delta = vt_encode_failure_counts().since(&before);
        // Other tests may count concurrently; at LEAST ours must be there.
        assert!(delta.get(VtFailureClass::Parameter) >= 2);
        assert!(delta.get(VtFailureClass::NotAvailableNow) >= 1);
    }

    #[test]
    fn a_healthy_encoder_is_left_alone() {
        let mut feed = Feed::new();
        for _ in 0..50 {
            assert_eq!(feed.healthy(), EncoderHealthAction::None);
        }
        assert_eq!(feed.detector.last_verdict(), Some(SampleVerdict::Healthy));
    }

    /// The #411 field shape: frames keep being pushed, none come out, and the
    /// process logs VideoToolbox failures the whole time.
    #[test]
    fn sustained_vt_failures_recreate_after_n_consecutive_samples() {
        let mut feed = Feed::new();
        for sample in 1..FAILING_SAMPLES_BEFORE_RECREATE {
            assert_eq!(feed.failing(), EncoderHealthAction::None, "sample {sample}");
            assert_eq!(feed.detector.failing_streak(), sample);
        }
        assert_eq!(
            feed.failing(),
            EncoderHealthAction::Recreate {
                attempt: 1,
                vt_failures: 30 * u64::from(FAILING_SAMPLES_BEFORE_RECREATE),
                dominant: VtFailureClass::Parameter,
            }
        );
    }

    #[test]
    fn a_healthy_sample_resets_the_streak() {
        let mut feed = Feed::new();
        for _ in 0..FAILING_SAMPLES_BEFORE_RECREATE - 1 {
            feed.failing();
        }
        assert_eq!(feed.healthy(), EncoderHealthAction::None);
        assert_eq!(feed.detector.failing_streak(), 0);
        for _ in 0..FAILING_SAMPLES_BEFORE_RECREATE - 1 {
            assert_eq!(feed.failing(), EncoderHealthAction::None);
        }
    }

    /// Starvation alone is bandwidth or the frame dropper, not a broken
    /// encoder: republishing would only add a gap.
    #[test]
    fn starvation_without_vt_failures_never_recreates() {
        let mut feed = Feed::new();
        for _ in 0..20 {
            assert_eq!(
                feed.interval(0, VtFailureCounts::default()),
                EncoderHealthAction::None
            );
        }
        assert_eq!(
            feed.detector.last_verdict(),
            Some(SampleVerdict::StarvedWithoutVtFailures)
        );
    }

    /// The failures come from ANOTHER encoder (say a share's) while this track
    /// keeps encoding: this track must not be republished.
    #[test]
    fn failures_attributed_elsewhere_do_not_recreate_a_track_that_encodes() {
        let mut feed = Feed::new();
        for _ in 0..20 {
            assert_eq!(
                feed.interval(55, VtFailureCounts::of(VtFailureClass::Parameter, 30)),
                EncoderHealthAction::None
            );
        }
    }

    /// The Level 3.1 clamp encodes 13 of 30 fps; that is degraded, not broken.
    #[test]
    fn a_level_clamped_encoder_is_not_starved() {
        let mut feed = Feed::new();
        for _ in 0..20 {
            assert_eq!(
                feed.interval(26, VtFailureCounts::of(VtFailureClass::Parameter, 5)),
                EncoderHealthAction::None
            );
        }
    }

    #[test]
    fn an_idle_source_neither_advances_nor_clears_the_streak() {
        let mut feed = Feed::new();
        feed.failing();
        feed.failing();
        // Camera paused: a few frames, no output, failures still logged.
        feed.pushed += MIN_PUSHED_FRAMES_PER_SAMPLE - 1;
        feed.failures.add(&VtFailureCounts::of(VtFailureClass::Parameter, 3));
        assert_eq!(feed.sample(), EncoderHealthAction::None);
        assert_eq!(feed.detector.last_verdict(), Some(SampleVerdict::Idle));
        assert_eq!(feed.detector.failing_streak(), 2);
        assert!(matches!(feed.failing(), EncoderHealthAction::Recreate { .. }));
    }

    #[test]
    fn a_stats_gap_rebaselines_instead_of_judging() {
        let mut feed = Feed::new();
        feed.failing();
        feed.pushed += 60;
        let at = feed.t0 + ENCODER_HEALTH_SAMPLE_INTERVAL * feed.step;
        feed.step += 1;
        assert_eq!(
            feed.detector.observe(EncoderHealthSample {
                pushed_frames: feed.pushed,
                encoded_frames: None,
                vt_failures: feed.failures,
                at,
            }),
            EncoderHealthAction::None
        );
        // The next sample is a fresh baseline, not a judged interval.
        assert_eq!(feed.failing(), EncoderHealthAction::None);
        assert_eq!(feed.detector.last_verdict(), Some(SampleVerdict::Baseline));
    }

    #[test]
    fn a_recreate_that_works_reports_recovered_once() {
        let mut feed = Feed::new();
        for _ in 0..FAILING_SAMPLES_BEFORE_RECREATE {
            feed.failing();
        }
        feed.recreated();
        // Baseline for the new sender, then the grace samples.
        assert_eq!(feed.healthy(), EncoderHealthAction::None);
        for _ in 0..POST_RECREATE_GRACE_SAMPLES {
            assert_eq!(feed.healthy(), EncoderHealthAction::None);
            assert_eq!(feed.detector.last_verdict(), Some(SampleVerdict::Baseline));
        }
        assert_eq!(
            feed.healthy(),
            EncoderHealthAction::Recovered { attempts: 1 }
        );
        assert_eq!(feed.healthy(), EncoderHealthAction::None);
    }

    /// The grace protects a republished track's slow first interval.
    #[test]
    fn the_post_recreate_grace_absorbs_a_slow_start() {
        let mut feed = Feed::new();
        for _ in 0..FAILING_SAMPLES_BEFORE_RECREATE {
            feed.failing();
        }
        feed.recreated();
        feed.failing(); // baseline
        for _ in 0..POST_RECREATE_GRACE_SAMPLES {
            assert_eq!(feed.failing(), EncoderHealthAction::None);
        }
        assert_eq!(feed.detector.failing_streak(), 0);
    }

    /// The field failure started ~21 s after a fresh publish. A recreate that
    /// buys 20 s and then fails again must not loop forever: past the flap
    /// guard the detector gives up for the track's lifetime.
    #[test]
    fn recreate_flapping_gives_up_after_the_window_budget() {
        let mut feed = Feed::new();
        let mut recreates = 0;
        let mut gave_up = None;
        for cycle in 0..10 {
            match feed.fail_until_action() {
                EncoderHealthAction::Recreate { attempt, .. } => {
                    recreates += 1;
                    assert_eq!(attempt, recreates);
                    feed.recreated();
                    // Works for ~20 s, then fails again.
                    feed.healthy();
                    for _ in 0..POST_RECREATE_GRACE_SAMPLES {
                        feed.healthy();
                    }
                    assert!(matches!(feed.healthy(), EncoderHealthAction::Recovered { .. }));
                    for _ in 0..6 {
                        feed.healthy();
                    }
                }
                EncoderHealthAction::GiveUp {
                    attempts,
                    vt_failures,
                    dominant,
                } => {
                    gave_up = Some((cycle, attempts, vt_failures, dominant));
                    break;
                }
                other => panic!("cycle {cycle}: unexpected {other:?}"),
            }
        }
        let (cycle, attempts, vt_failures, dominant) = gave_up.expect("never gave up");
        assert_eq!(cycle as u32, MAX_RECREATES_PER_WINDOW);
        assert_eq!(attempts, MAX_RECREATES_PER_WINDOW);
        assert!(vt_failures > 0);
        assert_eq!(dominant, VtFailureClass::Parameter);
        assert!(feed.detector.gave_up());
        // Sticky: nothing more for this track.
        for _ in 0..10 {
            assert_eq!(feed.failing(), EncoderHealthAction::None);
        }
    }

    #[test]
    fn recreates_spread_beyond_the_window_do_not_give_up() {
        let mut feed = Feed::new();
        for round in 0..(MAX_RECREATES_PER_WINDOW * 3) {
            let action = feed.fail_until_action();
            assert!(
                matches!(action, EncoderHealthAction::Recreate { .. }),
                "round {round}: {action:?}"
            );
            feed.recreated();
            feed.advance(RECREATE_WINDOW);
        }
        assert!(!feed.detector.gave_up());
    }

    /// A republish that itself fails still spends flap budget.
    #[test]
    fn failed_recreates_count_toward_the_guard() {
        let mut feed = Feed::new();
        let mut actions = Vec::new();
        for _ in 0..=MAX_RECREATES_PER_WINDOW {
            actions.push(feed.fail_until_action());
            feed.detector.note_recreate_failed();
        }
        for action in &actions[..MAX_RECREATES_PER_WINDOW as usize] {
            assert!(matches!(action, EncoderHealthAction::Recreate { .. }), "{action:?}");
        }
        assert!(matches!(
            actions.last(),
            Some(EncoderHealthAction::GiveUp { .. })
        ));
    }

    #[test]
    fn the_dominant_class_names_the_streak() {
        let mut feed = Feed::new();
        feed.interval(0, VtFailureCounts::of(VtFailureClass::NotAvailableNow, 40));
        feed.interval(0, VtFailureCounts::of(VtFailureClass::Parameter, 10));
        let action = feed.interval(0, VtFailureCounts::of(VtFailureClass::NotAvailableNow, 40));
        assert_eq!(
            action,
            EncoderHealthAction::Recreate {
                attempt: 1,
                vt_failures: 90,
                dominant: VtFailureClass::NotAvailableNow,
            }
        );
    }

    #[test]
    fn failure_counts_are_bucketed_for_telemetry() {
        assert_eq!(failure_count_bucket(0), "0");
        assert_eq!(failure_count_bucket(1), "1_99");
        assert_eq!(failure_count_bucket(99), "1_99");
        assert_eq!(failure_count_bucket(100), "100_999");
        assert_eq!(failure_count_bucket(58_000), "1000_plus");
        for class in VtFailureClass::ALL {
            assert!(!class.tag().is_empty());
            assert!(class.tag().chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'));
        }
    }
}
