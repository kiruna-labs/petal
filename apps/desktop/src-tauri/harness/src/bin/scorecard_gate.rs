//! CI-safe scorecard gate for SPEC §2.3 / §7.
//!
//! Unlike `petal-harness`, this binary performs no LiveKit or window I/O. It
//! reads an already-produced scorecard JSON, enforces the absolute p95
//! glass-to-glass ceiling, optionally compares against a baseline scorecard,
//! and exits nonzero on failure.

use clap::Parser;
use petal_harness::scorecard::{
    evaluate_absolute_thresholds, evaluate_gate, AbsoluteThresholds, GateThresholds, Scorecard,
};

#[derive(Parser, Debug)]
#[command(about = "Gate a Petal SPEC §7 scorecard without starting LiveKit")]
struct Args {
    /// Scorecard JSON to evaluate.
    #[arg(long)]
    scorecard: String,

    /// Absolute p95 glass-to-glass ceiling in milliseconds. Defaults to the
    /// SPEC §2.3 LAN promise.
    #[arg(long, default_value_t = 150.0)]
    max_p95_ms: f64,

    /// Optional baseline scorecard JSON for regression gating.
    #[arg(long)]
    baseline: Option<String>,
}

fn main() {
    let args = Args::parse();

    let scorecard = read_scorecard(&args.scorecard);
    let absolute = evaluate_absolute_thresholds(
        &scorecard,
        AbsoluteThresholds {
            max_p95_latency_ms: args.max_p95_ms,
        },
    );

    let mut passed = true;
    if absolute.passed {
        println!(
            "Absolute p95 latency gate: PASS (<= {:.2}ms)",
            args.max_p95_ms
        );
    } else {
        passed = false;
        println!(
            "Absolute p95 latency gate: FAIL (limit {:.2}ms)",
            args.max_p95_ms
        );
        for violation in &absolute.violations {
            println!(
                "  VIOLATION [{}] {}: {:.2}ms > {:.2}ms",
                violation.scenario_name, violation.metric, violation.current, violation.threshold
            );
        }
    }

    if let Some(baseline_path) = args.baseline {
        let baseline = read_scorecard(&baseline_path);
        let regression = evaluate_gate(&baseline, &scorecard, GateThresholds::default());
        if regression.passed {
            println!("Baseline regression gate: PASS");
        } else {
            passed = false;
            println!("Baseline regression gate: FAIL");
            for item in &regression.regressions {
                println!(
                    "  REGRESSION [{}] {}: {:.2} -> {:.2} ({:+.1}%, allowed {:.1}%)",
                    item.scenario_name,
                    item.metric,
                    item.baseline,
                    item.current,
                    item.pct_change * 100.0,
                    item.allowed_pct * 100.0
                );
            }
            for scenario in &regression.missing_scenarios {
                println!("  MISSING SCENARIO [{scenario}]");
            }
        }
    }

    if !passed {
        std::process::exit(1);
    }
}

fn read_scorecard(path: &str) -> Scorecard {
    let json = std::fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!("Failed to read scorecard {path}: {e}");
        std::process::exit(2);
    });
    Scorecard::from_json(&json).unwrap_or_else(|e| {
        eprintln!("Failed to parse scorecard {path}: {e}");
        std::process::exit(2);
    })
}
