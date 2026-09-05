use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// How much of the intended behavior a scenario's oracle actually observes.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub enum EvidenceBasis {
    HostEffect,
    ContentVerified,
    WireShape,
    LivenessProxy,
    Scaffold,
}

pub fn for_scenario(id: &str) -> EvidenceBasis {
    match id {
        // The native/web media oracle checks decoded content dimensions and fps.
        "SHARE-W2N-Q" => EvidenceBasis::ContentVerified,
        // These currently prove delivery/heartbeat shape, not the user-visible effect.
        "SHARE-N2W-Q" | "DRAW-N" | "TELE" => EvidenceBasis::WireShape,
        "CAM" | "AUD" => EvidenceBasis::LivenessProxy,
        // CAM-N2W reads the PIXELS the browser drew (canvas readback behind a
        // positive control) alongside the frame-advance counters, so a pass
        // means the tile was seen, not that a track existed (#815). Left to
        // fall through it would print "PASS (proxy -- not content-verified)",
        // understating the one thing the scenario exists to prove.
        "CAM-N2W" => EvidenceBasis::ContentVerified,
        // The oracle is host-ledger based by design, but #470 currently prevents
        // the live scenario from establishing the active share it needs.
        "RC-P1080" => EvidenceBasis::HostEffect,
        // native_peer::evaluate_independent_move samples real
        // CGWindowListCopyWindowInfo geometry before/after a programmatic
        // move and cross-checks the sharer's own window stayed put -- a
        // direct host-effect proof of the product's defining feature (a
        // real, independently movable native window), not a proxy for one.
        // This was previously falling through to Scaffold, mislabeling every
        // SHARE-N2N pass as "PASS (proxy -- not content-verified)" despite
        // the oracle being at least as strong as RC-P1080's host-ledger check.
        "SHARE-N2N" => EvidenceBasis::HostEffect,
        // RC-N2N's oracle reads host-side effects on the peer (AX replay
        // ledger + the sacrificial document's own text); RC-N2W's is delivery
        // only (the web harness's received-input ledger) -- deliberately a
        // tier apart (#819 review; falling through printed both as Scaffold).
        "RC-N2N" => EvidenceBasis::HostEffect,
        "RC-N2W" => EvidenceBasis::WireShape,
        _ => EvidenceBasis::Scaffold,
    }
}

pub fn is_baseline_eligible(basis: EvidenceBasis) -> bool {
    matches!(
        basis,
        EvidenceBasis::HostEffect | EvidenceBasis::ContentVerified
    )
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn current_macos_version() -> String {
    // Keep this comparison self-contained: LaunchServices-launched apps must
    // not depend on a shell command or Homebrew PATH for cockpit bookkeeping.
    #[cfg(target_os = "macos")]
    {
        use std::ffi::CStr;
        use std::os::raw::c_void;

        unsafe extern "C" {
            fn sysctlbyname(
                name: *const i8,
                oldp: *mut c_void,
                oldlenp: *mut usize,
                newp: *mut c_void,
                newlen: usize,
            ) -> i32;
        }

        let name = b"kern.osproductversion\0";
        let mut buffer = [0_i8; 64];
        let mut length = buffer.len();
        let result = unsafe {
            sysctlbyname(
                name.as_ptr().cast(),
                buffer.as_mut_ptr().cast(),
                &mut length,
                std::ptr::null_mut(),
                0,
            )
        };
        if result == 0 {
            if let Ok(version) = unsafe { CStr::from_ptr(buffer.as_ptr()) }.to_str() {
                return version.to_string();
            }
        }
    }
    std::env::consts::OS.to_string()
}

fn baseline_path(test_runs_root: &Path) -> PathBuf {
    test_runs_root.join("baseline.json")
}

fn latency_for<'a>(scorecard: &'a Value, id: &str) -> Option<&'a Value> {
    scorecard
        .get("scenarios")
        .and_then(Value::as_array)
        .and_then(|scenarios| {
            scenarios
                .iter()
                .find(|scenario| scenario.get("scenarioName").and_then(Value::as_str) == Some(id))
        })
        .and_then(|scenario| scenario.get("latency"))
}

fn p95_for(scorecard: &Value, id: &str) -> Option<f64> {
    latency_for(scorecard, id)
        .and_then(|latency| latency.get("p95Ms"))
        .and_then(Value::as_f64)
}

fn sample_count_for(scorecard: &Value, id: &str) -> Option<u64> {
    latency_for(scorecard, id)
        .and_then(|latency| latency.get("sampleCount"))
        .and_then(Value::as_u64)
}

fn baseline_scenarios(verdicts: &[Value], scorecard: &Value) -> Vec<Value> {
    verdicts
        .iter()
        .map(|result| {
            let id = result
                .get("scenarioId")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let basis = for_scenario(id);
            let raw_verdict = result
                .get("verdict")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            json!({
                "id": id,
                "verdict": verdict_label(raw_verdict, basis),
                "evidenceBasis": basis,
                "p95Ms": p95_for(scorecard, id),
                "sampleCount": sample_count_for(scorecard, id),
            })
        })
        .collect()
}

/// Compare this run with the per-machine baseline and update the baseline when
/// safe. Regressions are diagnostic: they remain in `baselineComparison` and do
/// not change cockpit pass/fail, which must continue to describe this run's
/// actual scenario verdicts rather than a comparison against another machine
/// or software version.
pub fn compare_baseline(
    test_runs_root: &Path,
    selector: &str,
    verdicts: &[Value],
    scorecard: &Value,
    petal_version: &str,
) -> Value {
    let path = baseline_path(test_runs_root);
    let environment = json!({
        "petalVersion": petal_version,
        "macosVersion": current_macos_version(),
    });
    let scenarios = baseline_scenarios(verdicts, scorecard);
    let mut comparison = json!({
        "baselinePath": path,
        "baselineAgeMs": Value::Null,
        "environmentDrift": [],
        "regressions": [],
        "insufficientData": [],
        "baselineWritten": false,
    });

    let baseline = fs::read_to_string(&path)
        .ok()
        .and_then(|contents| serde_json::from_str::<Value>(&contents).ok());
    if let Some(baseline) = baseline.as_ref() {
        let created = baseline
            .get("createdAtUnixMs")
            .and_then(Value::as_u64)
            .unwrap_or_else(now_ms);
        comparison["baselineAgeMs"] = json!(now_ms().saturating_sub(created));
        for key in ["petalVersion", "macosVersion"] {
            let previous = baseline.get("environment").and_then(|env| env.get(key));
            if previous.is_some() && previous != environment.get(key) {
                comparison["environmentDrift"]
                    .as_array_mut()
                    .expect("environmentDrift is an array")
                    .push(json!(format!(
                        "{}: {} -> {}",
                        key,
                        previous.and_then(Value::as_str).unwrap_or("unknown"),
                        environment
                            .get(key)
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                    )));
            }
        }
        for current in &scenarios {
            let id = current
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let previous = baseline
                .get("scenarios")
                .and_then(Value::as_array)
                .and_then(|items| {
                    items
                        .iter()
                        .find(|item| item.get("id").and_then(Value::as_str) == Some(id))
                });
            let Some(previous) = previous else {
                comparison["insufficientData"]
                    .as_array_mut()
                    .expect("insufficientData is an array")
                    .push(json!(format!(
                        "{id}: INSUFFICIENT DATA for p95 comparison (baseline sampleCount: not measured; current sampleCount: {})",
                        current
                            .get("sampleCount")
                            .and_then(Value::as_u64)
                            .map(|count| count.to_string())
                            .unwrap_or_else(|| "not measured".to_string()),
                    )));
                continue;
            };
            if previous.get("verdict").and_then(Value::as_str) == Some("PASS")
                && current.get("verdict").and_then(Value::as_str) != Some("PASS")
            {
                comparison["regressions"]
                    .as_array_mut()
                    .expect("regressions is an array")
                    .push(json!(format!("{id}: pass -> {}", current["verdict"])));
            }
            if previous.get("evidenceBasis") != current.get("evidenceBasis") {
                comparison["regressions"]
                    .as_array_mut()
                    .expect("regressions is an array")
                    .push(json!(format!(
                        "{id}: evidence basis degraded ({} -> {})",
                        previous["evidenceBasis"], current["evidenceBasis"]
                    )));
            }
            let old_p95 = previous.get("p95Ms").and_then(Value::as_f64);
            let new_p95 = current.get("p95Ms").and_then(Value::as_f64);
            let old_sample_count = previous.get("sampleCount").and_then(Value::as_u64);
            let new_sample_count = current.get("sampleCount").and_then(Value::as_u64);
            let old_has_samples = old_sample_count.is_some_and(|count| count > 0);
            let new_has_samples = new_sample_count.is_some_and(|count| count > 0);
            match (old_p95, new_p95) {
                (Some(old_p95), Some(new_p95)) if old_has_samples && new_has_samples => {
                    if new_p95 > old_p95 * 1.2 {
                        comparison["regressions"]
                            .as_array_mut()
                            .expect("regressions is an array")
                            .push(json!(format!("{id}: p95 >20% ({old_p95} -> {new_p95} ms)")));
                    }
                }
                _ => {
                    comparison["insufficientData"]
                        .as_array_mut()
                        .expect("insufficientData is an array")
                        .push(json!(format!(
                            "{id}: INSUFFICIENT DATA for p95 comparison (baseline sampleCount: {}; current sampleCount: {})",
                            old_sample_count
                                .map(|count| count.to_string())
                                .unwrap_or_else(|| "not measured".to_string()),
                            new_sample_count
                                .map(|count| count.to_string())
                                .unwrap_or_else(|| "not measured".to_string()),
                        )));
                }
            }
        }
    } else {
        for current in &scenarios {
            let id = current
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let sample_count = current.get("sampleCount").and_then(Value::as_u64);
            comparison["insufficientData"]
                .as_array_mut()
                .expect("insufficientData is an array")
                .push(json!(format!(
                    "{id}: INSUFFICIENT DATA for p95 comparison (baseline sampleCount: not measured; current sampleCount: {})",
                    sample_count
                        .map(|count| count.to_string())
                        .unwrap_or_else(|| "not measured".to_string()),
                )));
        }
    }

    comparison["regressionVerdict"] = json!(if !comparison["regressions"]
        .as_array()
        .expect("regressions is an array")
        .is_empty()
    {
        "REGRESSION"
    } else if !comparison["insufficientData"]
        .as_array()
        .expect("insufficientData is an array")
        .is_empty()
    {
        "INSUFFICIENT DATA"
    } else {
        "NO REGRESSION"
    });

    // Per-scenario eligibility, not all-or-nothing: a run mixing tiers (e.g.
    // Quick's own LivenessProxy/WireShape scenarios alongside a
    // HostEffect/ContentVerified one) should still let the strong-evidence
    // scenarios accumulate a baseline. The old all-or-nothing gate (every
    // scenario in the run must qualify) made baseline-writing structurally
    // unreachable for Quick tier -- CAM/AUD/DRAW-N/TELE/SHARE-N2W-Q can never
    // all be HostEffect/ContentVerified by design, so SHARE-W2N-Q's own
    // qualifying passes never got recorded either. `selector` is no longer a
    // gate (a direct single-scenario run, e.g. `--test-case=SHARE-N2N`, is
    // just as trustworthy per-scenario as a `full`-tier run) but is kept for
    // provenance in the written record.
    let eligible_scenarios: Vec<Value> = scenarios
        .iter()
        .filter(|scenario| {
            matches!(
                scenario.get("evidenceBasis").and_then(Value::as_str),
                Some("HostEffect") | Some("ContentVerified")
            ) && scenario.get("verdict").and_then(Value::as_str) == Some("PASS")
        })
        .cloned()
        .collect();
    if !eligible_scenarios.is_empty() {
        let mut merged = baseline.clone().unwrap_or_else(|| json!({"scenarios": []}));
        merged["createdAtUnixMs"] = json!(now_ms());
        merged["environment"] = environment;
        merged["lastUpdatedBySelector"] = json!(selector);
        let merged_scenarios = merged["scenarios"]
            .as_array_mut()
            .expect("baseline scenarios is an array");
        let updated_ids: Vec<Value> = eligible_scenarios
            .iter()
            .map(|scenario| scenario["id"].clone())
            .collect();
        for scenario in eligible_scenarios {
            let id = scenario["id"].as_str().unwrap_or("unknown");
            if let Some(existing) = merged_scenarios
                .iter_mut()
                .find(|item| item["id"].as_str() == Some(id))
            {
                *existing = scenario;
            } else {
                merged_scenarios.push(scenario);
            }
        }
        if let Ok(contents) = serde_json::to_string_pretty(&merged) {
            if fs::create_dir_all(test_runs_root).is_ok()
                && fs::write(&path, format!("{contents}\n")).is_ok()
            {
                comparison["baselineWritten"] = json!(true);
                comparison["baselineScenariosUpdated"] = json!(updated_ids);
            }
        }
    } else {
        comparison["baselineSkippedReason"] = json!(
            "no scenario in this run was both PASS and HostEffect/ContentVerified-tier evidence"
        );
    }
    comparison
}

fn verdict_label(verdict: &str, basis: EvidenceBasis) -> String {
    let verdict = match verdict {
        "pass" => "PASS",
        "test-fail" | "infra-fail" => "FAIL",
        "skipped" => "SKIPPED",
        "cancelled" => "CANCELLED",
        other => other,
    };
    if verdict == "PASS" && !is_baseline_eligible(basis) {
        "PASS (proxy — not content-verified)".to_string()
    } else {
        verdict.to_string()
    }
}

/// Build the additive, machine-readable conclusion stored in run.jsonl.
pub fn from_verdicts(verdicts: &[Value], aborted: bool) -> Value {
    from_verdicts_with_baseline(verdicts, aborted, None)
}

pub fn from_verdicts_with_baseline(
    verdicts: &[Value],
    aborted: bool,
    baseline_comparison: Option<Value>,
) -> Value {
    let scenarios: Vec<Value> = verdicts
        .iter()
        .map(|event| {
            let payload = event.get("payload").unwrap_or(event);
            let id = payload
                .get("scenarioId")
                .or_else(|| event.get("scenarioId"))
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let basis = for_scenario(id);
            let verdict = payload
                .get("verdict")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            json!({
                "scenarioId": id,
                "verdict": verdict_label(verdict, basis),
                "evidenceBasis": basis,
                "detail": payload.get("message").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    let not_checked: Vec<Value> = scenarios
        .iter()
        .filter(|scenario| scenario["verdict"] == "SKIPPED" || scenario["verdict"] == "CANCELLED")
        .map(|scenario| {
            json!({
                "scenarioId": scenario["scenarioId"],
                "reason": scenario["detail"],
            })
        })
        .collect();
    let mut conclusion = json!({
        "status": if aborted { "aborted" } else { "complete" },
        "message": if aborted { "run aborted before verdict" } else { "scenario conclusions recorded" },
        "scenarios": scenarios,
        "notChecked": not_checked,
    });
    if let Some(comparison) = baseline_comparison {
        conclusion["baselineComparison"] = comparison;
    }
    conclusion
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_pass_is_mechanically_labeled_and_not_baseline_eligible() {
        let result = from_verdicts(
            &[json!({
                "scenarioId": "DRAW-N",
                "verdict": "pass",
                "message": "journal delivered"
            })],
            false,
        );
        assert_eq!(
            result["scenarios"][0]["verdict"],
            "PASS (proxy — not content-verified)"
        );
        assert_eq!(result["scenarios"][0]["evidenceBasis"], "WireShape");
        assert!(!is_baseline_eligible(for_scenario("DRAW-N")));
    }

    #[test]
    fn aborted_conclusion_is_explicit() {
        let result = from_verdicts(&[], true);
        assert_eq!(result["status"], "aborted");
        assert_eq!(result["message"], "run aborted before verdict");
        assert!(result["notChecked"].is_array());
    }

    fn scorecard(p95_ms: f64, sample_count: u64) -> Value {
        json!({"scenarios": [{"scenarioName": "SHARE-W2N-Q", "latency": {"p95Ms": p95_ms, "sampleCount": sample_count}}]})
    }

    fn verdict(verdict: &str, basis: &str) -> Value {
        json!({"scenarioId": "SHARE-W2N-Q", "verdict": verdict, "evidenceBasis": basis})
    }

    #[test]
    fn baseline_comparison_detects_drift_and_all_regression_classes() {
        let root = tempfile_dir();
        fs::write(
            root.join("baseline.json"),
            json!({
                "createdAtUnixMs": 1,
                "environment": {"petalVersion": "old", "macosVersion": "old-os"},
                "scenarios": [{"id": "SHARE-W2N-Q", "verdict": "PASS", "evidenceBasis": "HostEffect", "p95Ms": 100.0, "sampleCount": 1}]
            }).to_string(),
        ).unwrap();
        let result = compare_baseline(
            &root,
            "SHARE-W2N-Q",
            &[verdict("TEST-FAIL", "WireShape")],
            &scorecard(130.0, 1),
            "new",
        );
        assert_eq!(result["environmentDrift"].as_array().unwrap().len(), 2);
        assert_eq!(result["regressions"].as_array().unwrap().len(), 3);
        assert_eq!(result["baselineWritten"], false);
    }

    #[test]
    fn narrow_eligible_run_updates_its_own_entry_without_clobbering_others() {
        // Per-scenario eligibility (not the old all-or-nothing gate): a
        // single-scenario run for an eligible scenario writes/updates just
        // that scenario's baseline entry, leaving any other pre-existing
        // scenario entries (e.g. from an earlier `full` run) untouched.
        let root = tempfile_dir();
        let original = json!({
            "createdAtUnixMs": 1,
            "environment": {"petalVersion": "old", "macosVersion": "old"},
            "scenarios": [{"id": "OTHER", "verdict": "PASS", "evidenceBasis": "HostEffect", "p95Ms": 1.0}]
        });
        fs::write(root.join("baseline.json"), original.to_string()).unwrap();
        let result = compare_baseline(
            &root,
            "SHARE-W2N-Q",
            &[verdict("PASS", "ContentVerified")],
            &scorecard(1.0, 1),
            "new",
        );
        assert_eq!(result["baselineWritten"], true);
        assert_eq!(result["baselineScenariosUpdated"], json!(["SHARE-W2N-Q"]));
        let written: Value =
            serde_json::from_str(&fs::read_to_string(root.join("baseline.json")).unwrap()).unwrap();
        let scenarios = written["scenarios"].as_array().unwrap();
        assert_eq!(
            scenarios.len(),
            2,
            "OTHER must survive alongside the new entry"
        );
        assert!(scenarios
            .iter()
            .any(|s| s["id"] == "OTHER" && s["evidenceBasis"] == "HostEffect"));
        assert!(scenarios
            .iter()
            .any(|s| s["id"] == "SHARE-W2N-Q" && s["sampleCount"] == 1));
    }

    #[test]
    fn mixed_tier_run_only_records_the_eligible_scenario() {
        // Reproduces Quick tier's actual shape: a HostEffect/ContentVerified
        // scenario passing alongside WireShape/LivenessProxy scenarios that
        // can never qualify by design. The old all-or-nothing gate made this
        // permanently ineligible; per-scenario eligibility must still record
        // the one scenario that earned it.
        let root = tempfile_dir();
        let verdicts = [
            verdict("PASS", "ContentVerified"),
            json!({"scenarioId": "DRAW-N", "verdict": "PASS", "evidenceBasis": "WireShape"}),
            json!({"scenarioId": "CAM", "verdict": "PASS", "evidenceBasis": "LivenessProxy"}),
        ];
        let result = compare_baseline(&root, "quick", &verdicts, &scorecard(1.0, 1), "new");
        assert_eq!(result["baselineWritten"], true);
        assert_eq!(result["baselineScenariosUpdated"], json!(["SHARE-W2N-Q"]));
        let written: Value =
            serde_json::from_str(&fs::read_to_string(root.join("baseline.json")).unwrap()).unwrap();
        assert_eq!(written["scenarios"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn run_with_no_eligible_scenario_writes_nothing() {
        let root = tempfile_dir();
        let result = compare_baseline(
            &root,
            "quick",
            &[json!({"scenarioId": "TELE", "verdict": "PASS", "evidenceBasis": "WireShape"})],
            &scorecard(1.0, 1),
            "new",
        );
        assert_eq!(result["baselineWritten"], false);
        assert!(result["baselineSkippedReason"].is_string());
        assert!(!root.join("baseline.json").exists());
    }

    #[test]
    fn share_n2n_is_host_effect_tier_not_scaffold() {
        assert_eq!(for_scenario("SHARE-N2N"), EvidenceBasis::HostEffect);
        assert!(is_baseline_eligible(for_scenario("SHARE-N2N")));
        assert_eq!(
            verdict_label("pass", for_scenario("SHARE-N2N")),
            "PASS",
            "a real host-effect pass must not be mechanically downgraded to a proxy label"
        );
    }

    #[test]
    fn empty_latency_measurement_is_insufficient_data_not_no_regression() {
        let root = tempfile_dir();
        fs::write(
            root.join("baseline.json"),
            json!({
                "createdAtUnixMs": 1,
                "environment": {},
                "scenarios": [{
                    "id": "SHARE-W2N-Q",
                    "verdict": "PASS",
                    "evidenceBasis": "ContentVerified",
                    "p95Ms": 100.0,
                    "sampleCount": 4,
                }]
            })
            .to_string(),
        )
        .unwrap();

        let result = compare_baseline(
            &root,
            "SHARE-W2N-Q",
            &[verdict("PASS", "ContentVerified")],
            &json!({"scenarios": [{"scenarioName": "SHARE-W2N-Q", "latency": null}]}),
            "new",
        );

        assert_eq!(result["regressionVerdict"], "INSUFFICIENT DATA");
        assert_eq!(
            result["insufficientData"],
            json!(["SHARE-W2N-Q: INSUFFICIENT DATA for p95 comparison (baseline sampleCount: 4; current sampleCount: not measured)"])
        );
        assert!(result["regressions"].as_array().unwrap().is_empty());
    }

    #[test]
    fn measured_zero_latency_is_not_insufficient_data() {
        let root = tempfile_dir();
        fs::write(
            root.join("baseline.json"),
            json!({
                "createdAtUnixMs": 1,
                "environment": {},
                "scenarios": [{
                    "id": "SHARE-W2N-Q",
                    "verdict": "PASS",
                    "evidenceBasis": "ContentVerified",
                    "p95Ms": 0.0,
                    "sampleCount": 4,
                }]
            })
            .to_string(),
        )
        .unwrap();

        let result = compare_baseline(
            &root,
            "SHARE-W2N-Q",
            &[verdict("PASS", "ContentVerified")],
            &scorecard(0.0, 4),
            "new",
        );

        assert_eq!(result["regressionVerdict"], "NO REGRESSION");
        assert!(result["insufficientData"].as_array().unwrap().is_empty());
    }

    fn tempfile_dir() -> PathBuf {
        // `now_ms()` alone collides under parallel test execution: two tests
        // calling this within the same millisecond get the SAME directory,
        // so one test's baseline.json write races another's read, producing
        // "trailing characters" JSON parse errors or a mismatched-content
        // assertion failure (confirmed live -- flaky only under `cargo test`'s
        // default parallelism, always passed under --test-threads=1). A
        // process-wide atomic counter guarantees uniqueness even within the
        // same millisecond, independent of thread scheduling.
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "petal-cockpit-baseline-{}-{}-{n}",
            std::process::id(),
            now_ms()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
