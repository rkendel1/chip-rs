use std::fs;
use std::path::Path;

use chip_core::{
    CapabilityDecisionState, CapabilityId, DecisionStateError, EvidenceState, GraphStateToken,
    ImpactState, InputValue,
};
use sha2::{Digest, Sha256};

const GRAPH: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const OTHER_GRAPH: &str = "sha256:fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

fn state(
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

fn minimal() -> CapabilityDecisionState {
    state(
        "cap.a",
        GRAPH,
        EvidenceState::KnownStale,
        ImpactState::Impacted,
    )
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn encoding_is_deterministic_and_locked_by_a_golden() {
    assert_eq!(minimal().canonical_bytes(), minimal().canonical_bytes());
    // "chip.decision.v1" NUL | len 5, "cap.a" | graph digest | stale=02 impacted=01 | 0 inputs
    let expected = format!(
        "{}00{}{}{}{}",
        hex(b"chip.decision.v1"),
        "00000005",
        hex(b"cap.a"),
        &GRAPH["sha256:".len()..],
        "020100000000",
    );
    assert_eq!(hex(&minimal().canonical_bytes()), expected);
    assert_eq!(
        minimal().state_token().as_str(),
        "sha256:4f98b2ee6f3250f6c2e97c87ad71ef7e2c41adff3c1ac7793f61c96b38b33fe6",
        "golden token; update deliberately if the encoding version changes"
    );
}

#[test]
fn the_token_is_the_sha256_of_the_canonical_bytes() {
    let s = minimal().with_input("n", InputValue::Integer(3));
    let expected: [u8; 32] = Sha256::digest(s.canonical_bytes()).into();
    assert_eq!(s.state_digest(), expected);
    assert_eq!(
        s.state_token().as_str(),
        format!("sha256:{}", hex(&expected))
    );

    let mut reused = Vec::new();
    s.write_canonical(&mut reused);
    assert_eq!(reused, s.canonical_bytes());
}

#[test]
fn input_order_and_construction_path_do_not_matter() {
    let a = minimal()
        .with_input("zeta", InputValue::Integer(-7))
        .with_input("alpha", InputValue::Text("x".into()))
        .with_input("mid", InputValue::Bool(true));
    let b = minimal()
        .with_input("mid", InputValue::Bool(true))
        .with_input("alpha", InputValue::Text("x".into()))
        .with_input("zeta", InputValue::Integer(-7));
    assert_eq!(a.canonical_bytes(), b.canonical_bytes());
    assert_eq!(a.state_token(), b.state_token());
    assert_eq!(a.to_wire_json(), b.to_wire_json());
    assert_eq!(a, b);
}

#[test]
fn same_state_same_token_and_any_change_changes_it() {
    let base = minimal();
    assert_eq!(base.state_token(), minimal().state_token());

    let variants = [
        state(
            "cap.b",
            GRAPH,
            EvidenceState::KnownStale,
            ImpactState::Impacted,
        ),
        state(
            "cap.a",
            OTHER_GRAPH,
            EvidenceState::KnownStale,
            ImpactState::Impacted,
        ),
        state(
            "cap.a",
            GRAPH,
            EvidenceState::KnownValid,
            ImpactState::Impacted,
        ),
        state(
            "cap.a",
            GRAPH,
            EvidenceState::Unknown,
            ImpactState::Impacted,
        ),
        state(
            "cap.a",
            GRAPH,
            EvidenceState::KnownStale,
            ImpactState::Unchanged,
        ),
        minimal().with_input("n", InputValue::Integer(1)),
    ];
    let mut tokens = vec![base.state_token()];
    for v in &variants {
        assert_ne!(v.state_token(), base.state_token());
        tokens.push(v.state_token());
    }
    tokens.sort();
    tokens.dedup();
    assert_eq!(tokens.len(), 7, "every variant has its own token");
}

#[test]
fn length_prefixes_make_the_encoding_unambiguous() {
    let one = minimal().with_input("ab", InputValue::Text("c".into()));
    let two = minimal().with_input("a", InputValue::Text("bc".into()));
    assert_ne!(one.canonical_bytes(), two.canonical_bytes());
    // Same payload bits, different types.
    let int = minimal().with_input("x", InputValue::Integer(1));
    let flag = minimal().with_input("x", InputValue::Bool(true));
    assert_ne!(int.state_token(), flag.state_token());
}

#[test]
fn graph_state_is_a_validated_fixed_size_token() {
    let token = GraphStateToken::parse(GRAPH).unwrap();
    assert_eq!(token.to_string(), GRAPH);
    assert_eq!(token.as_bytes().len(), 32);
    for bad in [
        "",
        "sha256:",
        "sha256:abc",
        "md5:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        &GRAPH.to_uppercase(),
        &format!("{GRAPH}0"),
        "sha256:zz23456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    ] {
        assert!(
            matches!(
                GraphStateToken::parse(bad),
                Err(DecisionStateError::InvalidGraphState(_))
            ),
            "{bad}"
        );
    }
}

#[test]
fn wire_names_round_trip_and_unknown_names_are_refused() {
    for e in [
        EvidenceState::KnownValid,
        EvidenceState::KnownStale,
        EvidenceState::Unknown,
    ] {
        assert_eq!(EvidenceState::from_wire_name(e.wire_name()), Ok(e));
    }
    for i in [ImpactState::Impacted, ImpactState::Unchanged] {
        assert_eq!(ImpactState::from_wire_name(i.wire_name()), Ok(i));
    }
    assert!(EvidenceState::from_wire_name("fresh").is_err());
    assert!(ImpactState::from_wire_name("maybe").is_err());
}

#[test]
fn the_json_form_is_stable_and_valid() {
    assert_eq!(
        minimal().to_wire_json(),
        format!(
            r#"{{"schema":"chip.decision.v1","capability":"cap.a","graph_state":"{GRAPH}","evidence":"stale","impact":"impacted"}}"#
        )
    );
    let with_inputs = minimal()
        .with_input("note", InputValue::Text("say \"hi\"\n\\ \u{1}".into()))
        .with_input("count", InputValue::Integer(-2))
        .with_input("flag", InputValue::Bool(false));
    let parsed: serde_json::Value = serde_json::from_str(&with_inputs.to_wire_json()).unwrap();
    assert_eq!(parsed["schema"], "chip.decision.v1");
    assert_eq!(parsed["inputs"]["note"], "say \"hi\"\n\\ \u{1}");
    assert_eq!(parsed["inputs"]["count"], -2);
    assert_eq!(parsed["inputs"]["flag"], false);
    let keys: Vec<_> = parsed["inputs"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    assert_eq!(keys, ["count", "flag", "note"]);
}

#[test]
fn the_minimal_state_is_tiny() {
    let s = minimal();
    let canonical = s.canonical_bytes().len();
    let json = s.to_wire_json().len();
    println!("Decision state: canonical {canonical} bytes, json {json} bytes");
    assert_eq!(canonical, 16 + 1 + 4 + 5 + 32 + 2 + 4);
    assert!(
        canonical < 100 && json < 250,
        "measured: {canonical} / {json}"
    );
}

#[test]
fn construction_needs_no_filesystem() {
    // An empty directory, no repository, no .git, no .chip: construction still works and
    // leaves nothing behind.
    let dir = std::env::temp_dir().join(format!("chip-decision-nofs-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let before = std::env::current_dir().unwrap();
    std::env::set_current_dir(&dir).unwrap();
    let token = minimal().state_token();
    let again = minimal()
        .with_input("k", InputValue::Bool(true))
        .to_wire_json();
    std::env::set_current_dir(before).unwrap();
    assert!(!again.is_empty());
    assert_eq!(token, minimal().state_token());
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn the_decision_state_module_is_isolated() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let source = fs::read_to_string(root.join("src/decision_state.rs")).unwrap();
    let code: String = source
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
        .to_lowercase();
    for banned in [
        "chip_graph",
        "chip-graph",
        "chip_compute",
        "fx_provider_http",
        "fx_core",
        "reqwest",
        "tokio",
        "laya",
        "candle",
        "wasmtime",
        "wasmer",
        "wasmi",
        "std::fs",
        "std::process",
        "std::net",
        "std::env",
        "std::thread",
        "std::time",
        "systemtime",
        "instant",
        "command::new",
        "serde_json",
    ] {
        assert!(
            !code.contains(banned),
            "decision_state.rs must not use {banned}"
        );
    }

    let manifest = fs::read_to_string(root.join("Cargo.toml")).unwrap();
    let runtime = manifest.split("[dev-dependencies]").next().unwrap();
    let dependencies: Vec<&str> = runtime
        .lines()
        .filter_map(|l| l.split_once('=').map(|(name, _)| name.trim()))
        .collect();
    for banned in [
        "chip-graph",
        "chip-compute",
        "fx-provider-http",
        "chip-local-ml",
        "chip-laya-reasoner",
        "chip-wasm-reasoner",
        "reqwest",
        "tokio",
        "candle",
        "syn",
        "toml",
        "serde",
        "serde_json",
    ] {
        assert!(
            !dependencies.contains(&banned),
            "chip-core must not depend on {banned}"
        );
    }
}
