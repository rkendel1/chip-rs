#![allow(dead_code)]
use chip_session_memory::*;
use std::path::Path;

pub fn big(tag: &str, n: usize) -> String {
    let mut s = String::new();
    while s.len() < n {
        s.push_str(&format!(
            "{tag}: error[E0308]: mismatched types at line {}\n",
            s.len()
        ));
    }
    s.truncate(n);
    s
}

pub fn obs(id: &str, summary: &str, payload: &str) -> NewObservation {
    NewObservation {
        id: id.into(),
        kind: "test".into(),
        provenance: Some(format!("exec-supplied-by-caller-{id}")),
        summary: summary.into(),
        payload: Some(payload.into()),
        pinned: false,
        superseded_by: None,
    }
}

pub fn r(kind: &str, id: &str) -> Reference {
    Reference {
        kind: kind.into(),
        id: id.into(),
    }
}

/// A nontrivial session: a task and plan, many observations with large payloads (some repeated),
/// failed attempts, a verified repair, test failures and a pass, a hypothesis, an escalation, a
/// pending decision, and a checkpoint. The last test of command `integration` is an unresolved
/// failure whose diagnostic is the only payload recovery still needs.
pub fn rich(root: &Path, id: &str) -> SessionMemory {
    rich_in::<Felt>(root, id)
}

/// The same session over any storage candidate.
pub fn rich_in<B: Backend>(root: &Path, id: &str) -> SessionMemory<B> {
    let mut m = SessionMemory::<B>::create_with(
        root,
        id,
        "make the integration tests pass without weakening any test",
    )
    .unwrap();
    m.put_task("t1", "fix route inheritance", false).unwrap();
    assert_eq!(
        m.set_plan(vec!["read".into(), "fix".into(), "retest".into()])
            .unwrap(),
        1
    );
    for i in 0..40 {
        m.record_observation(
            obs(
                &format!("o{i:03}"),
                &format!("read #{i}"),
                &big(&format!("o{i}"), 6000),
            ),
            true,
        )
        .unwrap();
    }
    // A repeated diagnostic supersedes the earlier one.
    m.record_observation(obs("o040", "same failure again", &big("o3", 6000)), true)
        .unwrap();
    m.supersede_observation("o003", "o040").unwrap();

    m.record_test_result("tr1", "unit", false, vec!["unit::a".into()], Some("o010"))
        .unwrap();
    m.record_attempt(
        "a1",
        "edit executor",
        Outcome::Failed,
        vec![r("obs", "o010")],
    )
    .unwrap();
    m.record_test_result("tr2", "unit", false, vec!["unit::a".into()], Some("o011"))
        .unwrap();
    m.record_attempt(
        "a2",
        "edit routes",
        Outcome::Succeeded,
        vec![r("obs", "o011")],
    )
    .unwrap();
    m.record_test_result("tr3", "unit", true, vec![], Some("o012"))
        .unwrap();
    m.record_repair(
        "rp1",
        "inherit settings from base",
        Outcome::Succeeded,
        Some("tr3"),
    )
    .unwrap();
    m.record_hypothesis(
        "h1",
        "unknown keys should warn",
        vec![r("obs", "o020")],
        Some(600),
    )
    .unwrap();
    m.record_hypothesis(
        "h2",
        "unknown keys should fail",
        vec![r("obs", "o021")],
        Some(400),
    )
    .unwrap();
    m.resolve_hypothesis("h2").unwrap();
    m.record_test_result(
        "tr4",
        "integration",
        false,
        vec!["integration::lenient".into()],
        Some("o030"),
    )
    .unwrap();
    m.record_escalation(
        "e1",
        "two conventions disagree; tests cannot arbitrate",
        vec!["a1".into(), "a2".into()],
        vec!["strict retry keys fail config_lenient".into()],
        vec!["route inheritance fixed".into()],
        vec!["warn or fail?".into()],
    )
    .unwrap();
    m.set_pending(Some(PendingAction {
        description: "decide warn vs fail for unknown [retry] keys".into(),
        decision_needed: true,
    }))
    .unwrap();
    m
}
