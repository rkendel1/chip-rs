//! PR13: the WASM boundary. Structured bytes in, structured verdict out, no host reach.

use chip_core::{
    CapabilityId, EvidenceState, InputValue, LocalReasoner, LocalReasoningResult, ReasoningError,
    ReasoningInput,
};
use chip_wasm_reasoner::{WasmLocalReasoner, decode_verdict, encode_input};

/// Tiny deterministic fixture. Reads the structured input: a wrong ABI version
/// escalates; stale evidence continues; anything else escalates.
const FIXTURE: &str = r#"
(module
  (memory (export "memory") 1)
  (data (i32.const 1024) "\00\08\00stale ok")
  (data (i32.const 1536) "\01\09\00uncertain")
  (data (i32.const 2048) "\01\0b\00bad version")
  (func (export "input_offset") (result i32) (i32.const 4096))
  (func (export "reason") (param $len i32) (result i32)
    (if (i32.ne (i32.load8_u (i32.const 4096)) (i32.const 1))
      (then (return (i32.const 2048))))
    (if (result i32) (i32.eq (i32.load8_u (i32.const 4097)) (i32.const 1))
      (then (i32.const 1024))
      (else (i32.const 1536)))))
"#;

fn module(wat_text: &str) -> Vec<u8> {
    wat::parse_str(wat_text).expect("fixture should be valid WAT")
}

fn input(evidence: EvidenceState) -> ReasoningInput {
    ReasoningInput {
        capability: CapabilityId::new("cap.a").unwrap(),
        inputs: [("label".to_string(), InputValue::Text("x".into()))].into(),
        evidence,
    }
}

fn reasoner() -> WasmLocalReasoner {
    WasmLocalReasoner::from_bytes(&module(FIXTURE)).unwrap()
}

#[test]
fn the_module_judges_from_structured_input() {
    let r = reasoner();
    assert_eq!(
        r.reason(&input(EvidenceState::KnownStale)).unwrap(),
        LocalReasoningResult::Continue {
            rationale: "stale ok".into()
        }
    );
    assert_eq!(
        r.reason(&input(EvidenceState::Unknown)).unwrap(),
        LocalReasoningResult::Escalate {
            reason: "uncertain".into()
        }
    );
    assert_eq!(
        r.reason(&input(EvidenceState::KnownValid)).unwrap(),
        LocalReasoningResult::Escalate {
            reason: "uncertain".into()
        }
    );
}

#[test]
fn input_is_a_versioned_binary_structure_not_a_prompt() {
    let bytes = encode_input(&input(EvidenceState::KnownStale)).unwrap();
    assert_eq!(bytes[0], 1, "abi version");
    assert_eq!(bytes[1], 1, "evidence state");
    assert_eq!(&bytes[2..4], &5u16.to_le_bytes());
    assert_eq!(&bytes[4..9], b"cap.a");
    assert_eq!(&bytes[9..11], &1u16.to_le_bytes(), "one input");
    // Deterministic encoding.
    assert_eq!(
        bytes,
        encode_input(&input(EvidenceState::KnownStale)).unwrap()
    );
    // Verdicts are decoded from structure, and malformed ones are rejected.
    assert_eq!(
        decode_verdict(b"\x00\x02\x00ok").unwrap(),
        LocalReasoningResult::Continue {
            rationale: "ok".into()
        }
    );
    assert!(decode_verdict(b"\x07\x00\x00").is_err());
    assert!(decode_verdict(b"\x00\x09\x00ok").is_err());
    assert!(decode_verdict(b"\x00").is_err());
}

#[test]
fn repeated_calls_are_deterministic() {
    let r = reasoner();
    let first = r.reason(&input(EvidenceState::KnownStale)).unwrap();
    for _ in 0..20 {
        assert_eq!(r.reason(&input(EvidenceState::KnownStale)).unwrap(), first);
    }
}

#[test]
fn a_module_cannot_import_any_host_capability() {
    for import in [
        r#"(import "wasi_snapshot_preview1" "fd_write" (func (param i32 i32 i32 i32) (result i32)))"#,
        r#"(import "wasi_snapshot_preview1" "path_open" (func (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))"#,
        r#"(import "wasi_snapshot_preview1" "proc_exit" (func (param i32)))"#,
        r#"(import "wasi_snapshot_preview1" "environ_get" (func (param i32 i32) (result i32)))"#,
        r#"(import "wasi_snapshot_preview1" "sock_recv" (func (param i32 i32 i32 i32 i32 i32) (result i32)))"#,
        r#"(import "env" "read_chip_state" (func (result i32)))"#,
    ] {
        let wasm = module(&format!(
            r#"(module {import} (memory (export "memory") 1)
                 (func (export "input_offset") (result i32) (i32.const 4096))
                 (func (export "reason") (param i32) (result i32) (i32.const 0)))"#
        ));
        match WasmLocalReasoner::from_bytes(&wasm) {
            Err(ReasoningError::Failed(message)) => {
                assert!(message.contains("imports"), "{message}")
            }
            Ok(_) => panic!("module with import was accepted: {import}"),
            Err(other) => panic!("{other:?}"),
        }
    }
}

#[test]
fn runaway_computation_is_bounded() {
    let wasm = module(
        r#"(module (memory (export "memory") 1)
             (func (export "input_offset") (result i32) (i32.const 4096))
             (func (export "reason") (param i32) (result i32) (loop $l (br $l)) (i32.const 0)))"#,
    );
    let r = WasmLocalReasoner::from_bytes(&wasm).unwrap();
    assert!(matches!(
        r.reason(&input(EvidenceState::Unknown)),
        Err(ReasoningError::Failed(_))
    ));
}

#[test]
fn state_does_not_persist_between_calls() {
    // The module counts calls in its own memory; each call gets a fresh instance,
    // so the count never rises above one.
    let wasm = module(
        r#"(module (memory (export "memory") 1)
             (data (i32.const 1024) "\00\01\00a")
             (data (i32.const 1536) "\01\01\00b")
             (func (export "input_offset") (result i32) (i32.const 4096))
             (func (export "reason") (param i32) (result i32)
               (i32.store8 (i32.const 100) (i32.add (i32.load8_u (i32.const 100)) (i32.const 1)))
               (if (result i32) (i32.eq (i32.load8_u (i32.const 100)) (i32.const 1))
                 (then (i32.const 1024)) (else (i32.const 1536)))))"#,
    );
    let r = WasmLocalReasoner::from_bytes(&wasm).unwrap();
    for _ in 0..3 {
        assert_eq!(
            r.reason(&input(EvidenceState::Unknown)).unwrap(),
            LocalReasoningResult::Continue {
                rationale: "a".into()
            }
        );
    }
}

#[test]
fn malformed_modules_and_verdicts_fail_closed() {
    assert!(WasmLocalReasoner::from_bytes(b"not wasm").is_err());
    // A verdict pointing outside memory is an error, never a panic or a Continue.
    let wasm = module(
        r#"(module (memory (export "memory") 1)
             (func (export "input_offset") (result i32) (i32.const 4096))
             (func (export "reason") (param i32) (result i32) (i32.const 65535)))"#,
    );
    let r = WasmLocalReasoner::from_bytes(&wasm).unwrap();
    assert!(r.reason(&input(EvidenceState::Unknown)).is_err());
    // Missing exports.
    let bare = module(r#"(module (memory (export "memory") 1))"#);
    assert!(
        WasmLocalReasoner::from_bytes(&bare)
            .unwrap()
            .reason(&input(EvidenceState::Unknown))
            .is_err()
    );
}

#[test]
fn the_module_can_drive_chip_through_the_core_trait() {
    use std::sync::Arc;
    let reasoner: Arc<dyn LocalReasoner> = Arc::new(reasoner());
    let verdict = reasoner.reason(&input(EvidenceState::KnownStale)).unwrap();
    assert!(matches!(verdict, LocalReasoningResult::Continue { .. }));
}

/// End to end: evidence first, then the WASM reasoner, with no model and no execution.
#[tokio::test]
async fn an_agent_consults_the_wasm_reasoner_only_after_evidence() {
    use chip_core::{Agent, Assessment, CapabilityRequest, ExecutionId, StateToken};
    use fx_core::{FxError, ModelProvider, ModelRequest, ModelResponse};
    use std::sync::Arc;

    struct NoModel;
    #[async_trait::async_trait]
    impl ModelProvider for NoModel {
        async fn complete(&self, _r: ModelRequest) -> Result<ModelResponse, FxError> {
            panic!("local reasoning must not call the model");
        }
    }
    let agent = Agent::new(Arc::new(NoModel)).with_local_reasoner(Arc::new(reasoner()));
    let request =
        CapabilityRequest::new(ExecutionId::new("e1"), CapabilityId::new("cap.a").unwrap());

    // Nothing known: the fixture is not confident, so it escalates.
    let unknown = agent
        .assess_evidence(&request, Some(&StateToken::new("F1")))
        .unwrap();
    assert_eq!(
        unknown,
        Assessment::Escalate {
            reason: "uncertain".into()
        }
    );
}
