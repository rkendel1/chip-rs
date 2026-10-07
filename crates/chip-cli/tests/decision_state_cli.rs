//! `chip-cli decision-state` is an inspection command over values passed in. It reads no
//! repository, initializes no graph, executes nothing and calls no model.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use chip_core::{
    CapabilityDecisionState, CapabilityId, EvidenceState, GraphStateToken, ImpactState,
};
use chip_graph::{CapabilityCatalog, analyze, capability_slice};

const GRAPH: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn run_in(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_chip-cli"))
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap()
}

fn empty_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("chip-decision-cli-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn text(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn renders_the_state_and_its_token_without_touching_anything() {
    let dir = empty_dir("render");
    let args = [
        "decision-state",
        "--capability",
        "cap.a",
        "--graph-state",
        GRAPH,
        "--evidence",
        "stale",
        "--impact",
        "impacted",
    ];
    let out = run_in(&dir, &args);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let expected = CapabilityDecisionState::new(
        CapabilityId::new("cap.a").unwrap(),
        GraphStateToken::parse(GRAPH).unwrap(),
        EvidenceState::KnownStale,
        ImpactState::Impacted,
    )
    .state_token();
    assert_eq!(
        text(&out),
        format!(
            "Capability:\n  cap.a\n\nGraphState:\n  {GRAPH}\n\nEvidence:\n  stale\n\nImpact:\n  impacted\n\nStateToken:\n  {}\n",
            expected.as_str()
        )
    );
    assert_eq!(
        out.stdout,
        run_in(&dir, &args).stdout,
        "byte-identical when repeated"
    );
    assert_eq!(
        fs::read_dir(&dir).unwrap().count(),
        0,
        "no .chip/, no files, nothing created"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn every_field_changes_the_token() {
    let dir = empty_dir("fields");
    let token = |cap: &str, graph: &str, evidence: &str, impact: &str| {
        let out = run_in(
            &dir,
            &[
                "decision-state",
                "--capability",
                cap,
                "--graph-state",
                graph,
                "--evidence",
                evidence,
                "--impact",
                impact,
            ],
        );
        assert!(out.status.success());
        text(&out).lines().last().unwrap().trim().to_string()
    };
    let other = format!("sha256:{}", "ab".repeat(32));
    let all = [
        token("cap.a", GRAPH, "stale", "impacted"),
        token("cap.b", GRAPH, "stale", "impacted"),
        token("cap.a", &other, "stale", "impacted"),
        token("cap.a", GRAPH, "valid", "impacted"),
        token("cap.a", GRAPH, "unknown", "impacted"),
        token("cap.a", GRAPH, "stale", "unchanged"),
    ];
    let mut unique = all.to_vec();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), all.len());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn an_invalid_state_renders_nothing_and_emits_no_token() {
    let dir = empty_dir("invalid");
    let good = [
        ("--capability", "cap.a"),
        ("--graph-state", GRAPH),
        ("--evidence", "stale"),
        ("--impact", "impacted"),
    ];
    let bad_values = [
        ("--capability", "Not Valid"),
        ("--graph-state", "sha256:abc"),
        ("--graph-state", "md5:0123"),
        ("--evidence", "fresh"),
        ("--impact", "maybe"),
    ];
    for (flag, value) in bad_values {
        let mut args = vec!["decision-state".to_string()];
        for (f, v) in good {
            args.push(f.to_string());
            args.push(if f == flag {
                value.to_string()
            } else {
                v.to_string()
            });
        }
        let out = run_in(&dir, &args.iter().map(String::as_str).collect::<Vec<_>>());
        assert!(!out.status.success(), "{flag} {value}");
        assert!(
            text(&out).is_empty(),
            "nothing is rendered on failure: {}",
            text(&out)
        );
        assert!(!String::from_utf8_lossy(&out.stderr).is_empty());
    }
    for missing in 0..good.len() {
        let mut args = vec!["decision-state".to_string()];
        for (i, (f, v)) in good.iter().enumerate() {
            if i != missing {
                args.push(f.to_string());
                args.push(v.to_string());
            }
        }
        let out = run_in(&dir, &args.iter().map(String::as_str).collect::<Vec<_>>());
        assert_eq!(out.status.code(), Some(2));
        assert!(text(&out).is_empty());
    }
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn the_benchmark_reports_a_baseline() {
    let dir = empty_dir("bench");
    let out = run_in(&dir, &["--benchmark-decision-state", "2000"]);
    assert!(out.status.success());
    let shown = text(&out);
    for needle in [
        "Iterations per stage: 2000",
        "median",
        "p95",
        "max",
        "Canonical state: 64 bytes",
        "construct (clones the id",
        "state digest (hash from state)",
        "construct + digest (hot path)",
        "decision_state_alloc",
    ] {
        assert!(shown.contains(needle), "missing {needle:?} in\n{shown}");
    }
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
    let _ = fs::remove_dir_all(&dir);
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dest = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_tree(&e.path(), &dest);
        } else {
            fs::copy(e.path(), dest).unwrap();
        }
    }
}

/// The adapter, in miniature: graph facts in, runtime state out. Only the token string
/// crosses; `chip-core` never sees the graph.
fn decision_token(repo: &Path, evidence: EvidenceState, impact: ImpactState) -> String {
    let graph = analyze(repo).unwrap();
    let catalog_text = fs::read_to_string(repo.join("capabilities.json")).unwrap();
    let catalog = CapabilityCatalog::from_json(&catalog_text, &graph).unwrap();
    let slice = capability_slice(&graph, &catalog, "cap.slice").unwrap();
    let graph_state = GraphStateToken::parse(slice.state_token().as_str()).unwrap();
    CapabilityDecisionState::new(
        CapabilityId::new("cap.slice").unwrap(),
        graph_state,
        evidence,
        impact,
    )
    .state_token()
    .as_str()
    .to_string()
}

#[test]
fn unrelated_repository_changes_do_not_move_the_decision_state() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../chip-graph/tests/slice_fixture");
    let repo = empty_dir("bridge");
    copy_tree(&fixture, &repo);
    let stale = |r: &Path| decision_token(r, EvidenceState::KnownStale, ImpactState::Impacted);
    let before = stale(&repo);

    // The repository changes, outside the capability's slice: same graph state, same token.
    fs::write(
        repo.join("a/src/three.rs"),
        "pub fn other() {}\npub fn more() {}\n",
    )
    .unwrap();
    fs::write(repo.join("a/src/extra.rs"), "pub fn extra() {}\n").unwrap();
    assert_eq!(stale(&repo), before);

    // A structural change inside the slice changes the graph state, hence the token.
    fs::write(
        repo.join("a/src/one.rs"),
        "use crate::two::helper;\n\npub fn run() {\n    helper();\n}\n",
    )
    .unwrap();
    assert_ne!(stale(&repo), before);

    // Evidence and impact are separate inputs to the same decision.
    let fresh = decision_token(&repo, EvidenceState::KnownValid, ImpactState::Impacted);
    let calm = decision_token(&repo, EvidenceState::KnownStale, ImpactState::Unchanged);
    assert_ne!(fresh, stale(&repo));
    assert_ne!(calm, stale(&repo));
    let _ = fs::remove_dir_all(&repo);
}
