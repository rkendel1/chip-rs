//! The work loop is built from existing abstractions. It does not know how work is performed, how a
//! model is reached, what the graph is, or what a learned model is.

use std::fs;
use std::path::Path;

fn source() -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/work.rs")).unwrap()
}

fn code() -> String {
    source()
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
        .to_lowercase()
}

#[test]
fn the_loop_reaches_nothing_outside_the_existing_boundaries() {
    let code = code();
    for banned in [
        "std::fs",
        "std::net",
        "std::process",
        "std::env",
        "command::new",
        "std::thread",
        "tokio",
        "reqwest",
        "hyper",
        "chip_compute",
        "chip-compute",
        "fx_provider",
        "chip_graph",
        "chip-graph",
        "chip_local_decision",
        "chip_wasm",
        "appport",
        "laya",
        "candle",
        "onnx",
        "wasmi",
        "read_dir",
        "walkdir",
        "glob",
        "serde",
    ] {
        assert!(!code.contains(banned), "work.rs must not mention {banned}");
    }
}

#[test]
fn the_loop_is_bounded_by_construction() {
    let code = code();
    assert!(code.contains("max_turns") && code.contains("max_executions"));
    // Exactly one `loop`, guarded at its head by the turn limit.
    assert_eq!(code.matches("loop {").count(), 1);
    assert!(code.contains("run.summary.turns >= spec.limits.max_turns"));
    assert!(code.contains("self.summary.executions >= self.spec.limits.max_executions"));
    // No retries, no recursion, no nested agent.
    for banned in ["retry", "retries", "spawn", "run_work(", "join_all"] {
        let count = code.matches(banned).count();
        // `run_work(` appears once: its own definition.
        assert!(
            count <= usize::from(banned == "run_work("),
            "unexpected {banned} in work.rs ({count})"
        );
    }
}

#[test]
fn the_learned_model_is_not_an_authority_for_autonomous_continue() {
    // chip-core cannot even name it, and its local decision source is the LocalWorkPolicy and
    // LocalReasoner boundaries only.
    let manifest =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml")).unwrap();
    for banned in [
        "chip-local-decision",
        "chip-local-ml",
        "chip-laya",
        "chip-wasm",
    ] {
        assert!(!manifest.contains(banned), "{banned}");
    }
    assert!(source().contains("not wired in anywhere"));
}

#[test]
fn model_calls_go_through_one_place() {
    let code = code();
    assert_eq!(
        code.matches(".model_turn(").count(),
        1,
        "exactly one call site for the model"
    );
    assert!(code.contains("fn escalate("));
}
