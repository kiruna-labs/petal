//! Network impairment profile config (SPEC.md §7 point 4).
//!
//! `tc netem` is Linux-only (it hooks the kernel's qdisc layer) -- there is
//! no macOS equivalent that can be driven headlessly/scriptably the same
//! way. This module does NOT attempt to apply real impairment on this
//! machine (that would need a real `tc` invocation, i.e. a live process,
//! which is exactly the class of thing this task's environment constraint
//! rules out even if we were on Linux). Instead it defines the **portable
//! profile parameters** a Linux CI runner would feed into `tc qdisc add ...
//! netem ...`, plus the pure translation from a profile to the literal
//! `tc` argument string, so:
//!
//! - the profile *data* (loss %, jitter, bandwidth cap, latency) is
//!   versioned, typed, and unit-testable here,
//! - the actual `tc netem` invocation is a one-line follow-up on a Linux
//!   runner (`std::process::Command::new("tc").args(profile.tc_args())`),
//!   not a redesign.
//!
//! Event profiles (network-switch, device-drop) are modeled as a scripted
//! sequence of `(profile, hold_duration)` steps rather than a single static
//! profile, so "flip to a different interface mid-call" is just "apply
//! profile A, wait, apply profile B" -- exercised by a real runner driving
//! `apply_at` timestamps against its own clock; this module only computes
//! *which* profile should be active at a given elapsed time, which is pure
//! and testable without ever shelling out.

use serde::{Deserialize, Serialize};

/// One static network condition: constant packet loss %, jitter (ms), and
/// an optional bandwidth cap (kbit/s). Maps directly onto `tc netem`'s own
/// `loss`/`delay ... jitter`/`rate` parameters.
///
/// `name` is an owned `String` (not `&'static str`) specifically so this
/// type can round-trip through `serde_json` (a scorecard/baseline file is
/// read back from disk into an owned value, which can't borrow a `'static`
/// lifetime from the deserializer) -- see `scorecard.rs`, which embeds
/// `impairment_profile` as a plain string field rather than this whole
/// struct, but `TimelineStep`/`EventProfile` below DO serialize the full
/// struct, so it needs to be independently (de)serializable too.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImpairmentProfile {
    pub name: String,
    /// Base one-way delay, milliseconds.
    pub delay_ms: u32,
    /// Delay jitter (+/-), milliseconds.
    pub jitter_ms: u32,
    /// Packet loss, percent (0.0-100.0).
    pub loss_pct: f32,
    /// Bandwidth cap, kbit/s. `None` = unconstrained.
    pub rate_kbit: Option<u32>,
}

impl ImpairmentProfile {
    pub fn perfect() -> Self {
        Self { name: "perfect".to_string(), delay_ms: 0, jitter_ms: 0, loss_pct: 0.0, rate_kbit: None }
    }

    /// Typical LTE: modest delay/jitter, negligible loss, generous but
    /// finite bandwidth.
    pub fn four_g() -> Self {
        Self { name: "4g".to_string(), delay_ms: 50, jitter_ms: 20, loss_pct: 0.5, rate_kbit: Some(12_000) }
    }

    /// SPEC.md §7's "lossy-wifi (2-8% loss + jitter)" -- picks the midpoint
    /// of that stated range.
    pub fn lossy_wifi() -> Self {
        Self { name: "lossy-wifi".to_string(), delay_ms: 20, jitter_ms: 30, loss_pct: 5.0, rate_kbit: None }
    }

    /// Constrained bandwidth, e.g. a saturated shared uplink.
    pub fn congested() -> Self {
        Self { name: "congested".to_string(), delay_ms: 100, jitter_ms: 50, loss_pct: 1.0, rate_kbit: Some(1_500) }
    }

    pub fn all_static() -> Vec<Self> {
        vec![Self::perfect(), Self::four_g(), Self::lossy_wifi(), Self::congested()]
    }

    pub fn by_name(name: &str) -> Option<Self> {
        Self::all_static().into_iter().find(|p| p.name == name)
    }

    /// The literal `tc qdisc ... netem ...` argument list a Linux runner
    /// would pass to apply this profile on an interface. Pure string
    /// formatting -- does not shell out. Caller is responsible for prefixing
    /// with `qdisc add dev <iface> root netem` (first apply) vs `qdisc
    /// change dev <iface> root netem` (updating an already-applied qdisc),
    /// since that verb depends on runner-side state this module doesn't
    /// track.
    pub fn tc_netem_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if self.delay_ms > 0 || self.jitter_ms > 0 {
            args.push("delay".to_string());
            args.push(format!("{}ms", self.delay_ms));
            if self.jitter_ms > 0 {
                args.push(format!("{}ms", self.jitter_ms));
            }
        }
        if self.loss_pct > 0.0 {
            args.push("loss".to_string());
            args.push(format!("{:.2}%", self.loss_pct));
        }
        if let Some(rate) = self.rate_kbit {
            args.push("rate".to_string());
            args.push(format!("{rate}kbit"));
        }
        args
    }
}

/// One step in an event profile's timeline: hold `profile` starting at
/// `apply_at_ms` (elapsed scenario time) until the next step (or scenario
/// end).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimelineStep {
    pub apply_at_ms: u64,
    pub profile: ImpairmentProfile,
}

/// A scripted sequence of impairment changes over the life of a scenario --
/// covers SPEC.md §7's "event profiles that mid-call flip the interface
/// (network-switch) or drop a device."
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventProfile {
    pub name: String,
    /// Steps, which MUST be sorted ascending by `apply_at_ms` and start at 0
    /// (enforced by `EventProfile::new`, not re-validated on every lookup).
    pub steps: Vec<TimelineStep>,
}

impl EventProfile {
    /// SPEC.md §7's "network-switch": start on `4g`-like conditions,
    /// degrade briefly to `congested` mid-call (simulating losing the good
    /// interface), then settle back to `perfect` (simulating landing on a
    /// new interface), e.g. Wi-Fi -> cellular fallback -> Ethernet.
    pub fn network_switch() -> Self {
        Self::new(
            "network-switch",
            vec![
                (0, ImpairmentProfile::four_g()),
                (10_000, ImpairmentProfile::congested()),
                (15_000, ImpairmentProfile::perfect()),
            ],
        )
        .expect("built-in profile is well-formed")
    }

    /// SPEC.md §7's "device-drop": perfect conditions, then a hard outage
    /// (100% loss, modeled as `loss_pct: 100.0` on top of a base profile)
    /// for a few seconds, then recovery -- simulating a device losing its
    /// network entirely and reconnecting.
    pub fn device_drop() -> Self {
        let outage = ImpairmentProfile {
            name: "outage".to_string(),
            delay_ms: 0,
            jitter_ms: 0,
            loss_pct: 100.0,
            rate_kbit: Some(1),
        };
        Self::new(
            "device-drop",
            vec![(0, ImpairmentProfile::perfect()), (8_000, outage), (13_000, ImpairmentProfile::perfect())],
        )
        .expect("built-in profile is well-formed")
    }

    pub fn new(
        name: impl Into<String>,
        steps: Vec<(u64, ImpairmentProfile)>,
    ) -> Result<Self, ImpairmentError> {
        if steps.is_empty() {
            return Err(ImpairmentError::Empty);
        }
        if steps[0].0 != 0 {
            return Err(ImpairmentError::MustStartAtZero);
        }
        for pair in steps.windows(2) {
            if pair[1].0 <= pair[0].0 {
                return Err(ImpairmentError::NotSorted);
            }
        }
        Ok(Self {
            name: name.into(),
            steps: steps
                .into_iter()
                .map(|(apply_at_ms, profile)| TimelineStep { apply_at_ms, profile })
                .collect(),
        })
    }

    /// Which profile should be active at `elapsed_ms` into the scenario.
    /// Pure lookup, no clock/process access -- a real runner drives this by
    /// polling its own elapsed time and diffing against the previously
    /// active profile to decide when to actually invoke `tc`.
    pub fn active_at(&self, elapsed_ms: u64) -> ImpairmentProfile {
        self.steps
            .iter()
            .rev()
            .find(|s| s.apply_at_ms <= elapsed_ms)
            .map(|s| s.profile.clone())
            .unwrap_or_else(|| self.steps[0].profile.clone())
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ImpairmentError {
    #[error("event profile must have at least one step")]
    Empty,
    #[error("event profile's first step must start at apply_at_ms = 0")]
    MustStartAtZero,
    #[error("event profile steps must be strictly ascending by apply_at_ms")]
    NotSorted,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_profiles_are_found_by_name() {
        assert_eq!(ImpairmentProfile::by_name("4g"), Some(ImpairmentProfile::four_g()));
        assert_eq!(ImpairmentProfile::by_name("nonexistent"), None);
    }

    #[test]
    fn perfect_profile_has_no_tc_args() {
        assert!(ImpairmentProfile::perfect().tc_netem_args().is_empty());
    }

    #[test]
    fn lossy_wifi_tc_args_contain_delay_jitter_and_loss() {
        let args = ImpairmentProfile::lossy_wifi().tc_netem_args();
        assert_eq!(
            args,
            vec!["delay", "20ms", "30ms", "loss", "5.00%"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn congested_tc_args_include_rate() {
        let args = ImpairmentProfile::congested().tc_netem_args();
        assert!(args.contains(&"rate".to_string()));
        assert!(args.contains(&"1500kbit".to_string()));
    }

    #[test]
    fn event_profile_must_start_at_zero() {
        let err = EventProfile::new("bad", vec![(10, ImpairmentProfile::perfect())]).unwrap_err();
        assert_eq!(err, ImpairmentError::MustStartAtZero);
    }

    #[test]
    fn event_profile_steps_must_be_sorted() {
        let err = EventProfile::new(
            "bad",
            vec![
                (0, ImpairmentProfile::perfect()),
                (5, ImpairmentProfile::four_g()),
                (5, ImpairmentProfile::congested()),
            ],
        )
        .unwrap_err();
        assert_eq!(err, ImpairmentError::NotSorted);
    }

    #[test]
    fn network_switch_timeline_resolves_correctly_at_each_stage() {
        let profile = EventProfile::network_switch();
        assert_eq!(profile.active_at(0).name, "4g");
        assert_eq!(profile.active_at(9_999).name, "4g");
        assert_eq!(profile.active_at(10_000).name, "congested");
        assert_eq!(profile.active_at(14_999).name, "congested");
        assert_eq!(profile.active_at(15_000).name, "perfect");
        assert_eq!(profile.active_at(999_999).name, "perfect");
    }

    #[test]
    fn device_drop_timeline_has_a_total_outage_window() {
        let profile = EventProfile::device_drop();
        assert_eq!(profile.active_at(0).name, "perfect");
        let outage = profile.active_at(9_000);
        assert_eq!(outage.name, "outage");
        assert_eq!(outage.loss_pct, 100.0);
        assert_eq!(profile.active_at(13_000).name, "perfect");
    }
}
