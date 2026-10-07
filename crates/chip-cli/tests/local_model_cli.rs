//! `chip-cli --benchmark-local-model` and `--evaluate-local-model` on the embedded model.

use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_chip-cli"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn the_benchmark_reports_initialization_warm_inference_and_end_to_end_with_tails() {
    let out = run(&["--benchmark-local-model", "2000"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    for needle in [
        "Artifact: 840 bytes",
        "75 parameters",
        "Resident model:",
        "Initialization",
        "model load (parse + checksum + validate)",
        "Warm inference",
        "feature extraction",
        "model inference (logits + threshold)",
        "decision mapping (baseline + guard)",
        "End to end",
        "learned: LocalDecider",
        "deterministic native",
        "median",
        "p95",
        "p99",
        "max",
        "Learned / deterministic (median)",
    ] {
        assert!(text.contains(needle), "missing {needle:?} in\n{text}");
    }
}

#[test]
fn the_corpus_evaluation_reports_every_policy_and_no_false_continue() {
    let out = run(&["--evaluate-local-model"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    for needle in [
        "in-sample",
        "Deterministic",
        "Learned (raw, unguarded)",
        "LearnedGuarded",
        "LearnedStrict",
        "false continue",
        "local decision coverage",
        "local_gain",
        "additional safe local decisions",
        "newly resolved:",
        "unsafe learned continues",
    ] {
        assert!(text.contains(needle), "missing {needle:?} in\n{text}");
    }
    assert!(!text.contains("FAILED"));
    assert_eq!(text.matches("false continues 0 ").count(), 4, "{text}");
}

#[test]
fn the_decision_corpus_report_leads_with_the_headline_and_appends_live_latency() {
    let out = run(&["--report-decision-corpus", "2000"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.starts_with("Additional safe local decisions: "),
        "{}",
        &text[..80.min(text.len())]
    );
    for needle in [
        "Corpus: 654 cases from generator chip.decision-corpus-gen.v1",
        "Pattern-held-out:",
        "Context-held-out:",
        "Learned raw:",
        "Learned strict:",
        "Learned guarded:",
        "Safe generalization rate:",
        "Hard negatives",
        "New safe local decisions",
        "Failed generalizations",
        "NATIVE INFERENCE (live",
        "median",
        "p95",
        "p99",
    ] {
        assert!(text.contains(needle), "missing {needle:?} in\n{text}");
    }
}
