//! Scorecard + CI regression gate (SPEC.md §7 point 5).
//!
//! Pure data + comparison logic: turns one or more scenario runs'
//! [`metrics::LatencyStats`]/[`metrics::FreezeStats`] into a serializable
//! [`ScenarioResult`], rolls a set of scenarios up into a [`Scorecard`], and
//! compares a scorecard against a baseline with a configurable regression
//! threshold. None of this touches the filesystem/network on its own --
//! callers (the `harness` binary) decide where the JSON is read from/written
//! to; this module only defines the shape and the comparison.

use serde::{Deserialize, Serialize};

use crate::metrics::{FreezeStats, LatencyStats};

/// Result of one scenario run (one participant count / impairment profile /
/// shares-per-bot combination).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioResult {
    pub scenario_name: String,
    /// Optional live-validation row covered by this scenario, e.g. "A3" in
    /// GitHub issue #28. Synthetic scenarios can leave this unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_id: Option<String>,
    /// Optional source issue covered by this scenario, e.g. "#236".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_issue: Option<String>,
    /// Optional coarse coverage bucket, such as "synthetic-media" or
    /// "remote-control-loopback".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage_kind: Option<String>,
    pub participant_count: u32,
    pub shares_per_bot: u32,
    pub impairment_profile: String,
    pub latency: LatencyStats,
    pub freeze: FreezeStats,
    /// Frames actually delivered per second over the measurement window
    /// (distinct from the publish-side target fps -- this is what arrived).
    pub delivered_fps: f64,
    pub delivered_width: u32,
    pub delivered_height: u32,
    /// Time to resume receiving frames after a scripted network event
    /// (SPEC.md §4.8 reconnect), if this scenario exercised one.
    pub reconnect_ms: Option<f64>,
}

/// A full run's rolled-up scorecard: every scenario's result plus summary
/// aggregates used for the regression gate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scorecard {
    pub generated_at_unix_ms: u64,
    pub scenarios: Vec<ScenarioResult>,
}

impl Scorecard {
    pub fn new(generated_at_unix_ms: u64, scenarios: Vec<ScenarioResult>) -> Self {
        Self {
            generated_at_unix_ms,
            scenarios,
        }
    }

    pub fn to_json_pretty(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }

    pub fn from_json(s: &str) -> serde_json::Result<Self> {
        serde_json::from_str(s)
    }

    fn find<'a>(&'a self, scenario_name: &str) -> Option<&'a ScenarioResult> {
        self.scenarios
            .iter()
            .find(|s| s.scenario_name == scenario_name)
    }
}

/// Regression thresholds, as fractional degradation allowed vs baseline
/// (e.g. `0.20` = up to 20% worse is tolerated before failing the gate).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct GateThresholds {
    pub max_p95_latency_regression: f64,
    pub max_freeze_count_regression: f64,
    pub max_fps_regression: f64,
}

impl Default for GateThresholds {
    fn default() -> Self {
        Self {
            max_p95_latency_regression: 0.20,
            max_freeze_count_regression: 0.50,
            max_fps_regression: 0.10,
        }
    }
}

/// One regression finding for a single scenario/metric.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Regression {
    pub scenario_name: String,
    pub metric: String,
    pub baseline: f64,
    pub current: f64,
    pub pct_change: f64,
    pub allowed_pct: f64,
}

/// Outcome of comparing a candidate scorecard against a baseline.
#[derive(Debug, Clone, Serialize)]
pub struct GateResult {
    pub passed: bool,
    pub regressions: Vec<Regression>,
    /// Scenarios present in the candidate but missing from the baseline
    /// (informational -- not itself a failure, e.g. a newly added scenario).
    pub new_scenarios: Vec<String>,
    /// Scenarios present in the baseline but missing from the candidate --
    /// this DOES fail the gate (a scenario silently dropped from the suite
    /// is exactly the kind of regression a coverage gate exists to catch).
    pub missing_scenarios: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct AbsoluteThresholds {
    pub max_p95_latency_ms: f64,
}

impl Default for AbsoluteThresholds {
    fn default() -> Self {
        Self {
            max_p95_latency_ms: 150.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ThresholdViolation {
    pub scenario_name: String,
    pub metric: String,
    pub threshold: f64,
    pub current: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AbsoluteGateResult {
    pub passed: bool,
    pub violations: Vec<ThresholdViolation>,
}

/// Enforce the SPEC §2.3 absolute glass-to-glass latency promise. This is
/// intentionally separate from [`evaluate_gate`]: a baseline comparison can
/// catch regressions, but the product claim also needs a hard ceiling that can
/// fail even when there is no prior baseline.
pub fn evaluate_absolute_thresholds(
    scorecard: &Scorecard,
    thresholds: AbsoluteThresholds,
) -> AbsoluteGateResult {
    let mut violations = Vec::new();

    for scenario in &scorecard.scenarios {
        if scenario.latency.p95_ms > thresholds.max_p95_latency_ms {
            violations.push(ThresholdViolation {
                scenario_name: scenario.scenario_name.clone(),
                metric: "p95_latency_ms".to_string(),
                threshold: thresholds.max_p95_latency_ms,
                current: scenario.latency.p95_ms,
            });
        }
    }

    AbsoluteGateResult {
        passed: violations.is_empty(),
        violations,
    }
}

/// Compare `candidate` against `baseline` using `thresholds`. Higher
/// latency/freeze-count and lower fps than baseline (beyond the allowed
/// threshold) are regressions; lower latency/freeze or higher fps never is.
pub fn evaluate_gate(
    baseline: &Scorecard,
    candidate: &Scorecard,
    thresholds: GateThresholds,
) -> GateResult {
    let mut regressions = Vec::new();
    let mut new_scenarios = Vec::new();
    let mut missing_scenarios = Vec::new();

    for cur in &candidate.scenarios {
        let Some(base) = baseline.find(&cur.scenario_name) else {
            new_scenarios.push(cur.scenario_name.clone());
            continue;
        };

        check_increase_regression(
            &mut regressions,
            &cur.scenario_name,
            "p95_latency_ms",
            base.latency.p95_ms,
            cur.latency.p95_ms,
            thresholds.max_p95_latency_regression,
        );
        check_increase_regression(
            &mut regressions,
            &cur.scenario_name,
            "freeze_count",
            base.freeze.freeze_count as f64,
            cur.freeze.freeze_count as f64,
            thresholds.max_freeze_count_regression,
        );
        check_decrease_regression(
            &mut regressions,
            &cur.scenario_name,
            "delivered_fps",
            base.delivered_fps,
            cur.delivered_fps,
            thresholds.max_fps_regression,
        );
    }

    for base in &baseline.scenarios {
        if candidate.find(&base.scenario_name).is_none() {
            missing_scenarios.push(base.scenario_name.clone());
        }
    }

    let passed = regressions.is_empty() && missing_scenarios.is_empty();

    GateResult {
        passed,
        regressions,
        new_scenarios,
        missing_scenarios,
    }
}

/// A metric where a higher current value than baseline is bad (latency,
/// freeze count). Flags a regression if `current` exceeds `baseline * (1 +
/// allowed_pct)`. Baseline == 0 is handled by falling back to an absolute
/// comparison (any nonzero current value on a zero baseline is a full
/// regression) so a `0/0` baseline doesn't produce a division-by-zero NaN
/// threshold that always passes.
fn check_increase_regression(
    out: &mut Vec<Regression>,
    scenario_name: &str,
    metric: &str,
    baseline: f64,
    current: f64,
    allowed_pct: f64,
) {
    let pct_change = pct_change(baseline, current);
    let limit = if baseline == 0.0 {
        0.0
    } else {
        baseline * (1.0 + allowed_pct)
    };
    let regressed = if baseline == 0.0 {
        current > 0.0
    } else {
        current > limit
    };
    if regressed {
        out.push(Regression {
            scenario_name: scenario_name.to_string(),
            metric: metric.to_string(),
            baseline,
            current,
            pct_change,
            allowed_pct,
        });
    }
}

/// A metric where a lower current value than baseline is bad (fps).
fn check_decrease_regression(
    out: &mut Vec<Regression>,
    scenario_name: &str,
    metric: &str,
    baseline: f64,
    current: f64,
    allowed_pct: f64,
) {
    let pct_change = pct_change(baseline, current);
    let limit = baseline * (1.0 - allowed_pct);
    if current < limit {
        out.push(Regression {
            scenario_name: scenario_name.to_string(),
            metric: metric.to_string(),
            baseline,
            current,
            pct_change,
            allowed_pct,
        });
    }
}

fn pct_change(baseline: f64, current: f64) -> f64 {
    if baseline == 0.0 {
        if current == 0.0 {
            0.0
        } else {
            f64::INFINITY
        }
    } else {
        (current - baseline) / baseline
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{FreezeStats, LatencyStats};

    fn scenario(name: &str, p95_ms: f64, freeze_count: usize, fps: f64) -> ScenarioResult {
        ScenarioResult {
            scenario_name: name.to_string(),
            row_id: None,
            source_issue: None,
            coverage_kind: None,
            participant_count: 3,
            shares_per_bot: 1,
            impairment_profile: "perfect".to_string(),
            latency: LatencyStats {
                sample_count: 100,
                min_ms: 10.0,
                avg_ms: 20.0,
                p50_ms: 18.0,
                p95_ms,
                max_ms: 40.0,
            },
            freeze: FreezeStats {
                freeze_count,
                total_missing_frames: freeze_count as u32,
                longest_freeze_us: 100_000,
                frames_received: 900,
            },
            delivered_fps: fps,
            delivered_width: 1280,
            delivered_height: 720,
            reconnect_ms: None,
        }
    }

    #[test]
    fn identical_scorecards_pass_with_no_regressions() {
        let baseline = Scorecard::new(0, vec![scenario("3p-perfect", 60.0, 0, 30.0)]);
        let candidate = Scorecard::new(1, vec![scenario("3p-perfect", 60.0, 0, 30.0)]);
        let result = evaluate_gate(&baseline, &candidate, GateThresholds::default());
        assert!(result.passed);
        assert!(result.regressions.is_empty());
    }

    #[test]
    fn latency_regression_beyond_threshold_fails_the_gate() {
        let baseline = Scorecard::new(0, vec![scenario("3p-perfect", 60.0, 0, 30.0)]);
        // 60ms -> 100ms is +66%, threshold default is 20%.
        let candidate = Scorecard::new(1, vec![scenario("3p-perfect", 100.0, 0, 30.0)]);
        let result = evaluate_gate(&baseline, &candidate, GateThresholds::default());
        assert!(!result.passed);
        assert_eq!(result.regressions.len(), 1);
        assert_eq!(result.regressions[0].metric, "p95_latency_ms");
    }

    #[test]
    fn latency_regression_within_threshold_passes() {
        let baseline = Scorecard::new(0, vec![scenario("3p-perfect", 60.0, 0, 30.0)]);
        // 60ms -> 70ms is +16.6%, under the 20% default threshold.
        let candidate = Scorecard::new(1, vec![scenario("3p-perfect", 70.0, 0, 30.0)]);
        let result = evaluate_gate(&baseline, &candidate, GateThresholds::default());
        assert!(result.passed);
    }

    #[test]
    fn fps_drop_beyond_threshold_fails_the_gate() {
        let baseline = Scorecard::new(0, vec![scenario("3p-perfect", 60.0, 0, 30.0)]);
        // 30fps -> 20fps is -33%, over the 10% default threshold.
        let candidate = Scorecard::new(1, vec![scenario("3p-perfect", 60.0, 0, 20.0)]);
        let result = evaluate_gate(&baseline, &candidate, GateThresholds::default());
        assert!(!result.passed);
        assert_eq!(result.regressions[0].metric, "delivered_fps");
    }

    #[test]
    fn freeze_count_increase_beyond_threshold_fails_the_gate() {
        let baseline = Scorecard::new(0, vec![scenario("3p-perfect", 60.0, 2, 30.0)]);
        // 2 -> 4 freezes is +100%, over the 50% default threshold.
        let candidate = Scorecard::new(1, vec![scenario("3p-perfect", 60.0, 4, 30.0)]);
        let result = evaluate_gate(&baseline, &candidate, GateThresholds::default());
        assert!(!result.passed);
        assert_eq!(result.regressions[0].metric, "freeze_count");
    }

    #[test]
    fn zero_baseline_freeze_count_any_regression_fails() {
        let baseline = Scorecard::new(0, vec![scenario("3p-perfect", 60.0, 0, 30.0)]);
        let candidate = Scorecard::new(1, vec![scenario("3p-perfect", 60.0, 1, 30.0)]);
        let result = evaluate_gate(&baseline, &candidate, GateThresholds::default());
        assert!(!result.passed);
    }

    #[test]
    fn missing_scenario_in_candidate_fails_the_gate() {
        let baseline = Scorecard::new(
            0,
            vec![
                scenario("3p-perfect", 60.0, 0, 30.0),
                scenario("8p-congested", 150.0, 1, 25.0),
            ],
        );
        let candidate = Scorecard::new(1, vec![scenario("3p-perfect", 60.0, 0, 30.0)]);
        let result = evaluate_gate(&baseline, &candidate, GateThresholds::default());
        assert!(!result.passed);
        assert_eq!(result.missing_scenarios, vec!["8p-congested".to_string()]);
    }

    #[test]
    fn new_scenario_in_candidate_is_informational_not_a_failure() {
        let baseline = Scorecard::new(0, vec![scenario("3p-perfect", 60.0, 0, 30.0)]);
        let candidate = Scorecard::new(
            1,
            vec![
                scenario("3p-perfect", 60.0, 0, 30.0),
                scenario("8p-lossy-wifi", 200.0, 3, 20.0),
            ],
        );
        let result = evaluate_gate(&baseline, &candidate, GateThresholds::default());
        assert!(result.passed);
        assert_eq!(result.new_scenarios, vec!["8p-lossy-wifi".to_string()]);
    }

    #[test]
    fn improvement_never_counts_as_a_regression() {
        let baseline = Scorecard::new(0, vec![scenario("3p-perfect", 100.0, 5, 20.0)]);
        let candidate = Scorecard::new(1, vec![scenario("3p-perfect", 30.0, 0, 30.0)]);
        let result = evaluate_gate(&baseline, &candidate, GateThresholds::default());
        assert!(result.passed);
    }

    #[test]
    fn scorecard_json_round_trips() {
        let scorecard = Scorecard::new(123, vec![scenario("3p-perfect", 60.0, 0, 30.0)]);
        let json = scorecard.to_json_pretty().unwrap();
        let parsed = Scorecard::from_json(&json).unwrap();
        assert_eq!(parsed.generated_at_unix_ms, 123);
        assert_eq!(parsed.scenarios.len(), 1);
        assert_eq!(parsed.scenarios[0].scenario_name, "3p-perfect");
        assert!((parsed.scenarios[0].latency.p95_ms - 60.0).abs() < 1e-9);
    }

    #[test]
    fn scenario_metadata_round_trips_when_present() {
        let mut result = scenario("3p-perfect", 60.0, 0, 30.0);
        result.row_id = Some("A3".to_string());
        result.source_issue = Some("#236".to_string());
        result.coverage_kind = Some("synthetic-media".to_string());
        let scorecard = Scorecard::new(123, vec![result]);

        let json = scorecard.to_json_pretty().unwrap();
        assert!(json.contains("\"row_id\""));
        let parsed = Scorecard::from_json(&json).unwrap();
        assert_eq!(parsed.scenarios[0].row_id.as_deref(), Some("A3"));
        assert_eq!(parsed.scenarios[0].source_issue.as_deref(), Some("#236"));
        assert_eq!(
            parsed.scenarios[0].coverage_kind.as_deref(),
            Some("synthetic-media")
        );
    }

    #[test]
    fn absolute_threshold_passes_when_p95_is_at_or_below_the_spec_ceiling() {
        let scorecard = Scorecard::new(
            0,
            vec![
                scenario("3p-perfect", 60.0, 0, 30.0),
                scenario("8p-perfect", 150.0, 0, 25.0),
            ],
        );
        let result = evaluate_absolute_thresholds(&scorecard, AbsoluteThresholds::default());
        assert!(result.passed);
        assert!(result.violations.is_empty());
    }

    #[test]
    fn absolute_threshold_fails_when_any_scenario_exceeds_the_spec_ceiling() {
        let scorecard = Scorecard::new(
            0,
            vec![
                scenario("3p-perfect", 60.0, 0, 30.0),
                scenario("8p-lossy-wifi", 151.0, 0, 25.0),
            ],
        );
        let result = evaluate_absolute_thresholds(&scorecard, AbsoluteThresholds::default());
        assert!(!result.passed);
        assert_eq!(result.violations.len(), 1);
        assert_eq!(result.violations[0].scenario_name, "8p-lossy-wifi");
        assert_eq!(result.violations[0].metric, "p95_latency_ms");
        assert!((result.violations[0].threshold - 150.0).abs() < 1e-9);
        assert!((result.violations[0].current - 151.0).abs() < 1e-9);
    }
}
