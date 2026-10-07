mod common;

use chip_core::{
    CapabilityDecisionState, CapabilityId, EvidenceState, GraphStateToken, ImpactState, InputValue,
};
use chip_reasoning_corpus::corpus;
use chip_wasm_decision::{Decision, DecisionResult, decide_bytes};
use chip_wasm_decision_host::{
    DecisionError, WasmDecisionEngine, WasmDecisionModule, decide_native,
};
use common::*;

fn engine() -> WasmDecisionEngine {
    WasmDecisionEngine::new(wasm_module()).expect("the built module satisfies the contract")
}

#[test]
fn end_to_end_state_to_decision_table() {
    let mut engine = engine();
    use EvidenceState::*;
    use ImpactState::*;
    let table = [
        (KnownValid, Unchanged, Decision::Continue),
        (KnownValid, Impacted, Decision::Escalate),
        (KnownStale, Unchanged, Decision::Escalate),
        (KnownStale, Impacted, Decision::Escalate),
        (Unknown, Unchanged, Decision::Escalate),
        (Unknown, Impacted, Decision::Escalate),
    ];
    for (evidence, impact, expected) in table {
        let state = minimal(evidence, impact);
        let wasm = engine.decide(&state).unwrap();
        assert_eq!(wasm.decision, expected, "{evidence:?} / {impact:?}");
        assert_eq!(wasm, decide_native(&state));
        assert_eq!(wasm, decide_bytes(&state.canonical_bytes()).unwrap());
        // Confidence is informational and never selects the decision.
        assert_eq!(wasm, DecisionResult::certain(expected));
    }
}

#[test]
fn changing_any_part_of_the_state_is_still_accepted_and_agrees_with_native() {
    let mut engine = engine();
    let base = minimal(EvidenceState::KnownValid, ImpactState::Unchanged);
    let other_graph = format!("sha256:{}", "ab".repeat(32));
    let mutations: Vec<CapabilityDecisionState> = vec![
        base.clone(),
        state(
            "other.capability-2_x",
            GRAPH,
            EvidenceState::KnownValid,
            ImpactState::Unchanged,
        ),
        state(
            "cap.a",
            &other_graph,
            EvidenceState::KnownValid,
            ImpactState::Unchanged,
        ),
        state(
            "cap.a",
            GRAPH,
            EvidenceState::KnownStale,
            ImpactState::Unchanged,
        ),
        state(
            "cap.a",
            GRAPH,
            EvidenceState::KnownValid,
            ImpactState::Impacted,
        ),
        with_inputs(base.clone()),
        base.clone().with_input("", InputValue::Text(String::new())),
        base.clone()
            .with_input("max", InputValue::Integer(i64::MAX))
            .with_input("min", InputValue::Integer(i64::MIN)),
        state(
            &"c".repeat(128),
            GRAPH,
            EvidenceState::KnownValid,
            ImpactState::Unchanged,
        ),
    ];
    for state in &mutations {
        let wasm = engine
            .decide(state)
            .expect("a well-formed state is accepted");
        assert_eq!(wasm, decide_native(state));
    }
}

#[test]
fn native_and_wasm_agree_across_the_reasoning_corpus() {
    let mut engine = engine();
    let cases = corpus();
    assert!(cases.len() >= 30);
    let mut escalations = 0;
    for case in &cases {
        let impact = match case.input.inputs.get("change_affects_capability") {
            Some(InputValue::Bool(true)) => ImpactState::Impacted,
            _ => ImpactState::Unchanged,
        };
        let mut state = CapabilityDecisionState::new(
            case.input.capability.clone(),
            GraphStateToken::parse(GRAPH).unwrap(),
            case.input.evidence,
            impact,
        );
        state.inputs = case.input.inputs.clone();
        let native = decide_native(&state);
        assert_eq!(engine.decide(&state).unwrap(), native, "case {}", case.id);
        assert_eq!(
            decide_bytes(&state.canonical_bytes()).unwrap(),
            native,
            "case {}",
            case.id
        );
        escalations += usize::from(native.decision == Decision::Escalate);
    }
    // Agreement is the test. Whether the baseline is *good* is a separate experiment.
    assert!(escalations > 0 && escalations < cases.len());
}

#[test]
fn the_persistent_instance_is_stable_and_not_poisoned_by_invalid_input() {
    let mut engine = engine();
    let valid = minimal(EvidenceState::KnownValid, ImpactState::Unchanged);
    let stale = minimal(EvidenceState::KnownStale, ImpactState::Unchanged);
    for i in 0..10_000 {
        let (state, expected) = if i % 2 == 0 {
            (&valid, Decision::Continue)
        } else {
            (&stale, Decision::Escalate)
        };
        assert_eq!(engine.decide(state).unwrap().decision, expected);
        if i % 1_000 == 0 {
            assert_eq!(
                engine.decide_bytes(b"garbage"),
                Err(DecisionError::InvalidState)
            );
        }
    }
}

fn expect_invalid(engine: &mut WasmDecisionEngine, bytes: &[u8], what: &str) {
    assert_eq!(
        engine.decide_bytes(bytes),
        Err(DecisionError::InvalidState),
        "{what}"
    );
    assert!(
        decide_bytes(bytes).is_err(),
        "{what}: native byte oracle agrees"
    );
}

#[test]
fn malformed_input_fails_closed_and_never_becomes_continue() {
    let mut engine = engine();
    let good = minimal(EvidenceState::KnownValid, ImpactState::Unchanged).canonical_bytes();
    assert_eq!(
        engine.decide_bytes(&good).unwrap().decision,
        Decision::Continue
    );
    let evidence_at = good.len() - 6;
    let impact_at = good.len() - 5;

    expect_invalid(&mut engine, &[], "empty");
    expect_invalid(&mut engine, &good[..good.len() - 1], "wrong length: short");
    let mut long = good.clone();
    long.push(0);
    expect_invalid(&mut engine, &long, "wrong length: trailing byte");
    expect_invalid(&mut engine, &vec![0u8; 5000], "oversized");

    let mut v2 = good.clone();
    v2[15] = b'2';
    expect_invalid(&mut engine, &v2, "unknown ABI version");
    let mut no_nul = good.clone();
    no_nul[16] = 1;
    expect_invalid(&mut engine, &no_nul, "bad version terminator");

    let mut upper = good.clone();
    upper[21] = b'C'; // "Cap.a"
    expect_invalid(&mut engine, &upper, "invalid capability character");
    let mut bad_utf8 = good.clone();
    bad_utf8[21] = 0xff;
    expect_invalid(&mut engine, &bad_utf8, "invalid capability encoding");
    let mut empty_cap = good.clone();
    empty_cap[17..21].copy_from_slice(&0u32.to_be_bytes());
    expect_invalid(&mut engine, &empty_cap, "empty capability");
    let mut huge_len = good.clone();
    huge_len[17..21].copy_from_slice(&u32::MAX.to_be_bytes());
    expect_invalid(&mut engine, &huge_len, "capability length beyond input");

    expect_invalid(&mut engine, &good[..30], "truncated graph token");

    for code in [0u8, 4, 7, 255] {
        let mut e = good.clone();
        e[evidence_at] = code;
        expect_invalid(&mut engine, &e, "invalid evidence value");
    }
    for code in [0u8, 3, 255] {
        let mut i = good.clone();
        i[impact_at] = code;
        expect_invalid(&mut engine, &i, "invalid impact value");
    }

    let with_bool = minimal(EvidenceState::KnownValid, ImpactState::Unchanged)
        .with_input("flag", InputValue::Bool(true))
        .canonical_bytes();
    let mut bad_bool = with_bool.clone();
    *bad_bool.last_mut().unwrap() = 2;
    expect_invalid(&mut engine, &bad_bool, "invalid bool");
    let mut lies_about_count = with_bool.clone();
    let count_at = lies_about_count.len() - (4 + 4 + 4 + 1 + 1) + 3;
    lies_about_count[count_at] = 9;
    expect_invalid(
        &mut engine,
        &lies_about_count,
        "input count larger than payload",
    );
}

#[test]
fn every_truncation_and_every_single_byte_mutation_agrees_with_the_oracle() {
    let mut engine = engine();
    for state in [
        minimal(EvidenceState::KnownValid, ImpactState::Unchanged),
        with_inputs(minimal(EvidenceState::KnownValid, ImpactState::Unchanged)),
    ] {
        let good = state.canonical_bytes();
        for cut in 0..good.len() {
            assert_eq!(
                engine.decide_bytes(&good[..cut]),
                Err(DecisionError::InvalidState),
                "cut {cut}"
            );
        }
        let mut continued = 0;
        for at in 0..good.len() {
            for value in [0u8, 1, 2, 3, 0x41, 0x7f, 0x80, 0xff] {
                let mut mutated = good.clone();
                mutated[at] = value;
                let wasm = engine.decide_bytes(&mutated);
                let native = decide_bytes(&mutated).map_err(|_| DecisionError::InvalidState);
                assert_eq!(wasm, native, "byte {at} = {value:#x}");
                if let Ok(result) = wasm {
                    // The only way to keep going is for the bytes to still be a valid Continue
                    // state: same length, evidence "valid" (byte 58) and impact "unchanged" (59).
                    if result.decision == Decision::Continue {
                        continued += 1;
                        assert_eq!(mutated.len(), good.len());
                        assert_eq!((mutated[58], mutated[59]), (1, 2), "byte {at} = {value:#x}");
                    }
                }
            }
        }
        assert!(
            continued > 0,
            "mutating the graph digest alone still yields a valid state"
        );
    }
}

#[test]
fn the_module_is_a_pure_calculator() {
    let module = WasmDecisionModule::compile(wasm_module()).unwrap();
    let _ = module.instantiate().unwrap();
    println!("Wasm module: {} bytes", wasm_module().len());
}

#[test]
fn the_state_the_module_sees_is_exactly_the_core_canonical_encoding() {
    // The same bytes chip-core produces are the bytes the module parses: a state with every
    // field populated round-trips to the same decision natively, as bytes, and in Wasm.
    let mut engine = engine();
    let state = with_inputs(CapabilityDecisionState::new(
        CapabilityId::new("deploy.service").unwrap(),
        GraphStateToken::parse(GRAPH).unwrap(),
        EvidenceState::KnownValid,
        ImpactState::Unchanged,
    ));
    assert_eq!(
        engine.decide_bytes(&state.canonical_bytes()),
        engine.decide(&state)
    );
    assert_eq!(engine.decide(&state).unwrap().decision, Decision::Continue);
}
