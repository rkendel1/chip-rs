#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use chip_core::{
    CapabilityDecisionState, CapabilityId, EvidenceState, GraphStateToken, ImpactState, InputValue,
};

pub const GRAPH: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The module, compiled with the size-optimized profile into a private target directory so the
/// outer cargo invocation is never involved. Fails loudly if the Wasm target is missing.
pub fn wasm_module() -> &'static [u8] {
    static MODULE: OnceLock<Vec<u8>> = OnceLock::new();
    MODULE.get_or_init(|| {
        let root = workspace_root();
        let target_dir = root.join("target/chip-wasm-decision-test");
        let output = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
            .current_dir(&root)
            .env("CARGO_TARGET_DIR", &target_dir)
            .args([
                "build",
                "-p",
                "chip-wasm-decision",
                "--target",
                "wasm32-unknown-unknown",
                "--profile",
                "wasm-decision",
                "--offline",
            ])
            .output()
            .expect("cargo should run");
        assert!(
            output.status.success(),
            "building the Wasm module failed (is the target installed? `rustup target add \
             wasm32-unknown-unknown`):\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        std::fs::read(
            target_dir.join("wasm32-unknown-unknown/wasm-decision/chip_wasm_decision.wasm"),
        )
        .expect("the built module exists")
    })
}

pub fn state(
    capability: &str,
    graph: &str,
    evidence: EvidenceState,
    impact: ImpactState,
) -> CapabilityDecisionState {
    CapabilityDecisionState::new(
        CapabilityId::new(capability).unwrap(),
        GraphStateToken::parse(graph).unwrap(),
        evidence,
        impact,
    )
}

pub fn minimal(evidence: EvidenceState, impact: ImpactState) -> CapabilityDecisionState {
    state("cap.a", GRAPH, evidence, impact)
}

pub fn all_evidence() -> [EvidenceState; 3] {
    [
        EvidenceState::KnownValid,
        EvidenceState::KnownStale,
        EvidenceState::Unknown,
    ]
}

pub fn all_impact() -> [ImpactState; 2] {
    [ImpactState::Unchanged, ImpactState::Impacted]
}

pub fn with_inputs(state: CapabilityDecisionState) -> CapabilityDecisionState {
    state
        .with_input("attempt", InputValue::Integer(-3))
        .with_input("dry_run", InputValue::Bool(true))
        .with_input("note", InputValue::Text("héllo".into()))
}
