//! PR9: the observation-to-turn path is composition, not hidden orchestration.

use std::fs;
use std::path::PathBuf;

fn read(file: &str) -> String {
    fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(file)).unwrap()
}

fn function(src: &str, signature: &str) -> String {
    let start = src
        .find(signature)
        .unwrap_or_else(|| panic!("{signature} not found"));
    let end = src[start..].find("\n    }\n").unwrap() + start;
    src[start..end].to_lowercase()
}

const SYSTEM_APIS: [&str; 20] = [
    "compute",
    "chip_compute",
    "chip-compute",
    "appport",
    "boundry",
    "feltdb",
    "attn",
    "pax",
    "shell",
    "std::process",
    "std::fs",
    "std::net",
    "http",
    "reqwest",
    "tokio",
    "openai",
    "anthropic",
    "sqlite",
    "journal",
    "checkpoint",
];

#[test]
fn observation_turn_path_has_no_system_or_persistence_references() {
    let lib = read("src/lib.rs");
    for signature in [
        "pub async fn turn_with_observations(",
        "pub async fn decide_with_observations(",
        "async fn model_turn(",
    ] {
        let body = function(&lib, signature);
        for forbidden in SYSTEM_APIS {
            assert!(
                !body.contains(forbidden),
                "{signature} must not reference {forbidden}"
            );
        }
    }
    let render = read("src/observation.rs").to_lowercase();
    for forbidden in SYSTEM_APIS {
        assert!(
            !render.contains(forbidden),
            "observation.rs must not reference {forbidden}"
        );
    }
}

#[test]
fn observation_turn_path_has_no_orchestration() {
    let lib = read("src/lib.rs");
    for signature in [
        "pub async fn turn_with_observations(",
        "pub async fn decide_with_observations(",
        "async fn model_turn(",
    ] {
        let body = function(&lib, signature);
        for forbidden in [
            "loop",
            "while ",
            "for _",
            "retry",
            "self.execute",
            "executor",
            "execute_capability",
            "observe(",
            "self.turn(",
            "self.run_turn",
            "validate_capability_request",
            "recurs",
        ] {
            assert!(
                !body.contains(forbidden),
                "{signature} must not contain {forbidden}"
            );
        }
    }
    // The only model call site in the turn path is the single call in model_turn.
    let body = function(&lib, "async fn model_turn(");
    assert_eq!(body.matches(".complete(").count(), 1);
}

#[test]
fn observations_come_only_from_results_never_from_model_text() {
    let src = read("src/observation.rs");
    assert!(
        !src.contains("ModelResponse"),
        "no path from model text to Observation"
    );
    for forbidden in [
        "autonomous",
        "run_until",
        "run_agent",
        "goal_loop",
        "continue_until",
    ] {
        assert!(
            !read("src/lib.rs").contains(forbidden),
            "{forbidden} must not exist"
        );
    }
}
