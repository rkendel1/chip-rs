mod common;

use std::fs;
use std::path::Path;

use chip_wasm_decision_host::{DecisionError, WasmDecisionEngine, WasmDecisionModule};
use common::*;

/// A hand-written module that satisfies (or deliberately violates) the ABI.
fn wat_module(version: &str, extra: &str, decide_body: &str) -> Vec<u8> {
    wat::parse_str(format!(
        r#"(module
  {extra}
  (memory (export "memory") 1)
  (data (i32.const 0) "{version}")
  (func (export "chip_abi_version_ptr") (result i32) (i32.const 0))
  (func (export "chip_abi_version_len") (result i32) (i32.const {len}))
  (func (export "chip_input_ptr") (result i32) (i32.const 1024))
  (func (export "chip_input_capacity") (result i32) (i32.const 4096))
  (func (export "chip_output_ptr") (result i32) (i32.const 8192))
  (func (export "chip_output_capacity") (result i32) (i32.const 8))
  (func (export "chip_decide") (param i32 i32 i32 i32) (result i32) {decide_body}))"#,
        len = version.len()
    ))
    .unwrap()
}

const GOOD_VERSION: &str = "chip.decision.v1";

fn writes(code: u8, returned: u32) -> String {
    format!("(i32.store8 (i32.const 8192) (i32.const {code})) (i32.const {returned})")
}

#[test]
fn a_conforming_hand_written_module_is_accepted() {
    let wasm = wat_module(GOOD_VERSION, "", &writes(1, 1));
    let mut engine = WasmDecisionEngine::new(&wasm).expect("conforming");
    let state = minimal(
        chip_core::EvidenceState::KnownValid,
        chip_core::ImpactState::Unchanged,
    );
    // It always answers "escalate" (1): the host reports exactly what the module said.
    assert_eq!(
        engine.decide(&state).unwrap().decision,
        chip_wasm_decision_host::Decision::Escalate
    );
}

#[test]
fn an_incompatible_or_unknown_abi_is_rejected() {
    for version in [
        "chip.decision.v2",
        "chip.decision.v0",
        "chip.decisio",
        "",
        "CHIP.DECISION.V1",
    ] {
        let wasm = wat_module(version, "", &writes(0, 1));
        match WasmDecisionEngine::new(&wasm) {
            Err(DecisionError::IncompatibleAbi(found)) => assert_eq!(found, version),
            other => panic!("{version:?} must be refused, got {:?}", other.err()),
        }
    }
}

#[test]
fn a_module_that_imports_anything_is_refused() {
    let wasm = wat_module(
        GOOD_VERSION,
        r#"(import "env" "read_file" (func))"#,
        &writes(0, 1),
    );
    assert!(matches!(
        WasmDecisionModule::compile(&wasm).err(),
        Some(DecisionError::ForbiddenImport(name)) if name == "env::read_file"
    ));
}

#[test]
fn a_module_missing_the_contract_is_refused() {
    let no_decide = wat::parse_str(r#"(module (memory (export "memory") 1))"#).unwrap();
    assert!(matches!(
        WasmDecisionEngine::new(&no_decide),
        Err(DecisionError::Module(_))
    ));
    assert!(matches!(
        WasmDecisionEngine::new(b"not wasm"),
        Err(DecisionError::Module(_))
    ));
    let no_memory = wat::parse_str(r#"(module (func (export "chip_decide") (param i32 i32 i32 i32) (result i32) (i32.const 0)))"#).unwrap();
    assert!(matches!(
        WasmDecisionEngine::new(&no_memory),
        Err(DecisionError::Module(_))
    ));
}

#[test]
fn nothing_a_module_returns_can_become_continue_by_accident() {
    let state = minimal(
        chip_core::EvidenceState::KnownValid,
        chip_core::ImpactState::Unchanged,
    );
    let run = |body: String| {
        let mut engine = WasmDecisionEngine::new(&wat_module(GOOD_VERSION, "", &body)).unwrap();
        engine.decide(&state)
    };
    // The module says Continue: that is a decision.
    assert!(run(writes(0, 1)).is_ok());
    // 255 is the reserved invalid result.
    assert_eq!(run(writes(255, 1)), Err(DecisionError::InvalidState));
    // Unknown codes, and a module that writes nothing or the wrong amount, are errors.
    for code in [2u8, 7, 100, 254] {
        assert!(
            matches!(run(writes(code, 1)), Err(DecisionError::BadOutput(_))),
            "code {code}"
        );
    }
    assert!(matches!(
        run(writes(0, 0)),
        Err(DecisionError::BadOutput(_))
    ));
    assert!(matches!(
        run(writes(0, 2)),
        Err(DecisionError::BadOutput(_))
    ));
    // A module that never terminates runs out of fuel instead of hanging the host.
    assert!(matches!(
        run("(loop $l (br $l)) (i32.const 0)".to_string()),
        Err(DecisionError::Trapped(_))
    ));
    // A trap is an error, not a decision.
    assert!(matches!(
        run("unreachable".to_string()),
        Err(DecisionError::Trapped(_))
    ));
}

#[test]
fn the_built_module_is_tiny_import_free_and_needs_no_allocator() {
    let wasm = wasm_module();
    let module = WasmDecisionModule::compile(wasm).unwrap();
    let _ = module;
    println!("Wasm module: {} bytes", wasm.len());
    assert!(
        wasm.len() < 16 * 1024,
        "decision module is {} bytes",
        wasm.len()
    );

    // Read the export section through the text of the binary: names only.
    let text = String::from_utf8_lossy(wasm);
    for exported in [
        "chip_decide",
        "chip_input_ptr",
        "chip_abi_version_ptr",
        "chip_abi_version_len",
        "memory",
    ] {
        assert!(text.contains(exported), "missing export {exported}");
    }
    for absent in ["alloc", "malloc", "realloc", "dealloc", "free"] {
        assert!(
            !text.contains(absent),
            "the module must not carry an allocator ({absent})"
        );
    }
    assert!(text.contains("chip.decision.v1"));
}

#[test]
fn the_decision_module_has_no_dependencies_and_none_of_the_forbidden_surface() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../chip-wasm-decision");
    let manifest = fs::read_to_string(root.join("Cargo.toml")).unwrap();
    let deps = manifest.split("[dependencies]").nth(1).unwrap();
    assert!(
        deps.lines()
            .all(|l| l.trim().is_empty() || l.trim().starts_with('#')),
        "chip-wasm-decision must have no dependencies:\n{deps}"
    );

    for file in ["lib.rs", "abi.rs", "model.rs"] {
        let source = fs::read_to_string(root.join("src").join(file)).unwrap();
        let code: String = source
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
            .to_lowercase();
        for banned in [
            "std::fs",
            "std::net",
            "std::process",
            "std::thread",
            "std::env",
            "std::time",
            "command::new",
            "tcpstream",
            "http",
            "reqwest",
            "tokio",
            "chip_core",
            "chip-core",
            "chip_graph",
            "chip-graph",
            "fx_core",
            "fx-core",
            "chip_compute",
            "appport",
            "laya",
            "candle",
            "onnx",
            "serde",
            "json",
            "alloc::",
            "vec!",
            "string::",
            "box::new",
        ] {
            assert!(!code.contains(banned), "{file} must not mention {banned}");
        }
    }
}
