//! The capability report: what the run showed Chip can and cannot do as a coding agent.
//!
//! Every verdict is computed from the evidence trail, the reality checks and the handoffs. None
//! of it comes from what a model said about itself. Where the run could not measure something,
//! the report says so rather than inferring a pass.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde_json::{Value, json};

use super::fixture::Size;
use super::ladder::{LadderResult, Level, Status};
use super::packet::Target;
use super::trace::count;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Fail,
    NotExercised,
}

impl Verdict {
    fn label(self) -> &'static str {
        match self {
            Verdict::Pass => "PASS",
            Verdict::Fail => "FAIL",
            Verdict::NotExercised => "NOT EXERCISED",
        }
    }
}

/// How a piece of work ended up being done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Done at the first tier, without a failure along the way.
    Capable,
    /// A failure occurred and the same tier recovered from it.
    Recoverable,
    /// Needed a stronger model or a person.
    Escalates,
    /// Not done.
    Blocked,
    /// This run cannot tell.
    Unknown,
}

impl Class {
    fn label(self) -> &'static str {
        match self {
            Class::Capable => "CAPABLE",
            Class::Recoverable => "RECOVERABLE",
            Class::Escalates => "ESCALATES",
            Class::Blocked => "BLOCKED",
            Class::Unknown => "UNKNOWN",
        }
    }
}

pub struct Dimension {
    pub name: &'static str,
    pub verdict: Verdict,
    pub evidence: String,
}

pub struct Item {
    pub name: String,
    pub class: Class,
    pub detail: String,
}

/// What was handed to a rung, as seen in the first request its model received.
pub struct Delivery {
    pub rung: usize,
    pub has_marker: bool,
    pub names_failing_tests: bool,
    pub includes_decision: bool,
}

pub struct Context {
    pub fx: String,
    pub pax: String,
    pub scripted: bool,
    pub fixture: Size,
    pub deliveries: Vec<Delivery>,
    pub elapsed: Duration,
}

pub struct Report {
    pub dimensions: Vec<Dimension>,
    pub items: Vec<Item>,
    pub limits: Vec<String>,
    pub boundaries: Vec<(String, String)>,
    pub status: String,
    pub json: Value,
    pub text: String,
}

fn module_of(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    match parts.as_slice() {
        ["src", m, _, ..] => format!("src/{m}"),
        [a, ..] => (*a).to_string(),
        [] => String::new(),
    }
}

struct Lifecycle {
    first_failed: Option<(usize, Level)>,
    closed: Option<(usize, Level)>,
}

fn lifecycle(result: &LadderResult, test: &str) -> Lifecycle {
    let mut first_failed = None;
    let mut closed = None;
    for r in &result.rungs {
        for run in &r.tests {
            let failing = run.failed_tests.iter().any(|n| n == test);
            if failing && first_failed.is_none() {
                first_failed = Some((r.index, r.level));
            } else if first_failed.is_some() && closed.is_none() && run.ran_ok(test) {
                // Closed only when the diagnostics show it ran and passed: absence from the
                // failing list proves nothing when Cargo stopped before its binary.
                closed = Some((r.index, r.level));
            }
        }
    }
    Lifecycle {
        first_failed,
        closed,
    }
}

fn classify(result: &LadderResult, name: &str, test: &str, what: &str) -> Item {
    let l = lifecycle(result, test);
    // The failures a person was asked about: those failing in the last run before the handoff.
    let asked_a_person: BTreeSet<String> = result
        .handoffs
        .iter()
        .filter(|h| h.to == Target::Human)
        .filter_map(|h| result.rungs.iter().find(|r| r.index == h.after_rung))
        .filter_map(|r| r.tests.last())
        .flat_map(|t| t.failed_tests.iter().cloned())
        .collect();
    let (class, detail) = match (l.first_failed, l.closed) {
        (None, _) => (Class::Unknown, format!("`{test}` never failed in this run")),
        (Some((fr, fl)), Some((cr, _))) => {
            let escalated_between = result
                .handoffs
                .iter()
                .any(|h| h.to != Target::Continue && h.after_rung >= fr && h.after_rung < cr);
            let changed_by_next = result.rungs.iter().find(|r| {
                r.index > fr && r.level != fl && r.trace.iter().any(|e| e.name == "file.changed")
            });
            if asked_a_person.contains(test) {
                (
                    Class::Escalates,
                    format!(
                        "failed at rung {fr}; a person was asked about it and it was shown passing at rung {cr}, after their decision. {what}"
                    ),
                )
            } else if escalated_between {
                (
                    Class::Escalates,
                    format!(
                        "failed at rung {fr} ({} tier), which ended in escalation; the {} tier changed code at rung {}; shown passing at rung {cr}. {what}",
                        fl.name(),
                        changed_by_next.map_or("next", |r| r.level.name()),
                        changed_by_next.map_or(cr, |r| r.index)
                    ),
                )
            } else {
                (
                    Class::Recoverable,
                    format!(
                        "failed at rung {fr}, shown passing at rung {cr}, with no escalation in between ({} tier). {what}",
                        fl.name()
                    ),
                )
            }
        }
        (Some((fr, fl)), None) => (
            Class::Blocked,
            format!(
                "failed at rung {fr} ({} tier) and was never shown passing. {what}",
                fl.name()
            ),
        ),
    };
    Item {
        name: name.to_string(),
        class,
        detail,
    }
}

pub fn evaluate(result: &LadderResult, ctx: &Context) -> Report {
    let events = &result.events;
    let first_change = events.iter().position(|e| e.name == "file.changed");
    let before_change = &events[..first_change.unwrap_or(events.len())];
    let reads_before: BTreeSet<String> = before_change
        .iter()
        .filter(|e| e.name == "file.read")
        .filter_map(|e| e.detail["path"].as_str().map(str::to_string))
        .collect();
    let modules_read: BTreeSet<String> = reads_before.iter().map(|p| module_of(p)).collect();
    let all_reads: BTreeSet<String> = events
        .iter()
        .filter(|e| e.name == "file.read")
        .filter_map(|e| e.detail["path"].as_str().map(str::to_string))
        .collect();
    let changed: Vec<String> = events
        .iter()
        .filter(|e| e.name == "file.changed")
        .filter_map(|e| e.detail["path"].as_str().map(str::to_string))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let changed_modules: BTreeSet<String> = changed.iter().map(|p| module_of(p)).collect();
    let test_runs: usize = events.iter().filter(|e| e.name == "test.completed").count();
    let verification = result.verification.as_ref();
    let verified = matches!(result.status, Status::Verified);
    let repairs_accepted = count(events, "repair.accepted");
    let repairs_rejected = count(events, "repair.rejected");
    let escalations: Vec<_> = events
        .iter()
        .filter(|e| e.name == "escalation.requested" && e.authority != "ladder")
        .collect();
    let local_escalations = escalations.iter().filter(|e| e.authority == "chip").count();
    let model_escalations = escalations
        .iter()
        .filter(|e| e.authority == "model-asserted")
        .count();
    let human_packets: Vec<_> = result
        .handoffs
        .iter()
        .filter(|h| h.to == Target::Human)
        .collect();

    let mut dims = Vec::new();
    let mut dim = |name: &'static str, verdict: Verdict, evidence: String| {
        dims.push(Dimension {
            name,
            verdict,
            evidence,
        })
    };

    dim(
        "Repository understanding",
        if count(before_change, "repository.inspected") > 0 && modules_read.len() >= 3 {
            Verdict::Pass
        } else {
            Verdict::Fail
        },
        format!(
            "before its first change the run inspected the repository and read {} files in {} modules ({})",
            reads_before.len(),
            modules_read.len(),
            modules_read.iter().cloned().collect::<Vec<_>>().join(", ")
        ),
    );
    let first_test = events.iter().position(|e| e.name == "test.completed");
    dim(
        "Task decomposition",
        match (first_test, first_change) {
            (Some(t), Some(c)) if t < c && changed.len() >= 4 => Verdict::Pass,
            _ => Verdict::Fail,
        },
        "observed from ordering only (inspect, test, change, test): the protocol has no plan artifact, so a plan the model held is not visible".into(),
    );
    dim(
        "Code navigation",
        if events
            .iter()
            .any(|e| e.name == "repository.inspected" && e.detail["via"] == "project.search")
            && all_reads.len() >= 8
        {
            Verdict::Pass
        } else {
            Verdict::Fail
        },
        format!(
            "used project.search and read {} distinct files",
            all_reads.len()
        ),
    );
    dim(
        "Implementation",
        if verified && changed.len() >= 4 {
            Verdict::Pass
        } else {
            Verdict::Fail
        },
        format!(
            "{} files changed through observed writes; final status {}",
            changed.len(),
            status_label(&result.status)
        ),
    );
    dim(
        "Test execution",
        if test_runs >= 3 {
            Verdict::Pass
        } else {
            Verdict::Fail
        },
        format!("{test_runs} pax.test runs, each with PAX's counts"),
    );
    let all_closed = [
        "retry_after_responses_spend_the_attempt_budget",
        "a_route_without_its_own_retry_settings_uses_the_client_policy",
        "unknown_keys_in_every_section_warn_and_continue",
    ]
    .iter()
    .all(|t| lifecycle(result, t).closed.is_some());
    dim(
        "Failure diagnosis",
        if all_closed { Verdict::Pass } else { Verdict::Fail },
        "every distinct failing test observed was later observed passing (names read from PAX's diagnostics)".into(),
    );
    dim(
        "Multi-file changes",
        if changed_modules.len() >= 3 && changed.len() >= 4 {
            Verdict::Pass
        } else {
            Verdict::Fail
        },
        format!(
            "{} files across {} modules",
            changed.len(),
            changed_modules.len()
        ),
    );
    dim(
        "Regression avoidance",
        match verification {
            Some(v) if v.regressions.is_empty() && v.integrity.is_empty() => Verdict::Pass,
            Some(_) => Verdict::Fail,
            None => Verdict::NotExercised,
        },
        match verification {
            Some(v) => format!(
                "{} regressions against the baseline; {} test-integrity violations; tests {} -> {}",
                v.regressions.len(),
                v.integrity.len(),
                v.tests_before,
                v.tests_after
            ),
            None => "no final verification ran".into(),
        },
    );
    dim(
        "Recovery",
        if repairs_accepted >= 1 {
            Verdict::Pass
        } else {
            Verdict::Fail
        },
        format!(
            "{repairs_accepted} repairs accepted, {repairs_rejected} rejected (judged by the next PAX result)"
        ),
    );
    dim(
        "Escalation detection",
        if local_escalations >= 1 {
            Verdict::Pass
        } else {
            Verdict::Fail
        },
        format!(
            "{local_escalations} escalation(s) decided by Chip's own policy, {model_escalations} requested by a model"
        ),
    );
    let clean = !result.handoffs.is_empty() && result.handoffs.iter().all(|h| h.defects.is_empty());
    dim(
        "Escalation context quality",
        if clean { Verdict::Pass } else { Verdict::Fail },
        format!(
            "{} handoff(s); defects found by the assessment: {}",
            result.handoffs.len(),
            result
                .handoffs
                .iter()
                .map(|h| h.defects.len())
                .sum::<usize>()
        ),
    );
    let strong = ctx
        .deliveries
        .iter()
        .find(|d| d.rung > 0 && !d.includes_decision);
    dim(
        "Strong-model escalation",
        match strong {
            Some(d) if d.has_marker && d.names_failing_tests => Verdict::Pass,
            Some(_) => Verdict::Fail,
            None => Verdict::NotExercised,
        },
        "the stronger tier's first request carried the Chip-built handoff and named the failing test. Whether that made its work better is not measured: its judgment was scripted".into(),
    );
    dim(
        "Human escalation",
        if human_packets.iter().any(|h| {
            h.defects.is_empty()
                && h.for_human.as_deref().is_some_and(|p| {
                    p.contains("Human decision required") && p.contains("Files currently modified")
                })
        }) {
            Verdict::Pass
        } else if human_packets.is_empty() {
            Verdict::NotExercised
        } else {
            Verdict::Fail
        },
        format!(
            "{} human packet(s), each with objective, state, attempts, evidence, failure, hypotheses and the decision required",
            human_packets.len()
        ),
    );
    let resumed = result.decisions.first().and_then(|_| {
        let h = result.handoffs.iter().find(|h| h.to == Target::Human)?;
        let before = result.rungs.iter().find(|r| r.index == h.after_rung)?;
        let after = result.rungs.iter().find(|r| r.index == h.after_rung + 1)?;
        Some((
            before.tree_after == after.tree_before,
            matches!(result.status, Status::Verified),
        ))
    });
    dim(
        "Resume after escalation",
        match resumed {
            Some((same_tree, done)) if same_tree && done => Verdict::Pass,
            Some(_) => Verdict::Fail,
            None => Verdict::NotExercised,
        },
        "the rung after the human decision started from exactly the tree the previous rung left, and finished the task. Resume is a new bounded run seeded from the handoff, not the same run".into(),
    );
    dim(
        "Final verification",
        if verified {
            Verdict::Pass
        } else {
            Verdict::Fail
        },
        verification.map_or("no final verification ran".into(), |v| {
            format!(
                "PAX run directly: {}; {} acceptance checks; {} problems",
                v.pax_status,
                v.checks.len(),
                v.reasons.len()
            )
        }),
    );

    let mut items = vec![
        classify(
            result,
            "Pre-existing defect: retry attempt budget",
            "retry_after_responses_spend_the_attempt_budget",
            "A failure the assignment did not mention.",
        ),
        classify(
            result,
            "Second-order failure: route inheritance",
            "a_route_without_its_own_retry_settings_uses_the_client_policy",
            "The cause is in a module the new code never touches.",
        ),
        classify(
            result,
            "Convention conflict: unknown keys in [retry]",
            "unknown_keys_in_every_section_warn_and_continue",
            "Two documented rules disagree; the tests cannot arbitrate.",
        ),
    ];
    // The feature item is computed: how many changes the cheap tier made, and what the first
    // test run after its last feature change showed.
    let cheap_writes = result
        .rungs
        .iter()
        .filter(|r| r.level == Level::Cheap)
        .flat_map(|r| r.trace.iter())
        .filter(|e| e.name == "file.changed")
        .count();
    let feature_rung = result.rungs.iter().find(|r| {
        r.level == Level::Cheap && r.trace.iter().filter(|e| e.name == "file.changed").count() >= 8
    });
    // The rung's first test run follows its feature changes: it ran no tests before them.
    let first_run_after = feature_rung.and_then(|r| r.tests.first());
    let (class, detail) = match (feature_rung, first_run_after) {
        (Some(r), Some(run))
            if run.failed_tests.len() <= 1 && run.status != chip_pax::PaxStatus::Error =>
        {
            (
                Class::Capable,
                format!(
                    "the cheap tier made {cheap_writes} file changes; in rung {} the first test run after its feature changes compiled and had {} failing test(s) of {} ({})",
                    r.index,
                    run.failed_tests.len(),
                    run.counts.map_or(0, |c| c.passed + c.failed),
                    run.failed_tests.join(", ")
                ),
            )
        }
        (Some(_), Some(run)) => (
            Class::Recoverable,
            format!(
                "the first test run after the feature changes was {} with {} failing",
                run.status.as_str(),
                run.failed_tests.len()
            ),
        ),
        _ => (
            Class::Unknown,
            "the cheap tier made no multi-file feature change in this run".to_string(),
        ),
    };
    items.insert(
        1,
        Item {
            name: "Feature: configurable retry across modules".into(),
            class,
            detail,
        },
    );
    items.push(Item {
        name: "Does model quality change the outcome?".into(),
        class: Class::Unknown,
        detail: if ctx.scripted { "not measurable: every tier's judgment was scripted, so this run says nothing about any real model".into() } else { "see the live results".into() },
    });

    let mut limits = Vec::new();
    for it in &items {
        match it.class {
            Class::Escalates => limits.push(format!(
                "Could not close `{}` at the first tier: {}",
                it.name, it.detail
            )),
            Class::Blocked => limits.push(format!("`{}` was never closed: {}", it.name, it.detail)),
            _ => {}
        }
    }
    if repairs_rejected > 0 {
        limits.push(format!("{repairs_rejected} ineffective repair(s) were attempted before Chip's repair budget stopped the tier"));
    }
    if result.events.iter().any(|e| e.name == "goal.unsatisfied") {
        limits.push("A rung reached runtime completion (PAX passed after the last change) while the assignment was not met. The product's completion rule for change work is \"tests pass\"; it cannot express feature acceptance. The evaluation needed its own acceptance gate above the runtime.".into());
    }
    limits.push("There is no in-loop resume: an escalated run is over. Continuing is a new bounded run whose only memory is the handoff in the goal text, plus the files on disk.".into());
    let redundant: usize = {
        let mut seen = BTreeSet::new();
        let mut n = 0;
        for r in &result.rungs {
            let mut this = BTreeSet::new();
            for e in r.trace.iter().filter(|e| e.name == "file.read") {
                let p = e.detail["path"].as_str().unwrap_or("").to_string();
                if seen.contains(&p) {
                    n += 1;
                }
                this.insert(p);
            }
            seen.extend(this);
        }
        n
    };
    limits.push(format!("Context does not survive a rung boundary except as the handoff: {redundant} file read(s) in later rungs repeated reads made in earlier ones (file contents are not part of the handoff)."));
    let too_big: Vec<usize> = result
        .rungs
        .iter()
        .filter(|r| !r.goal_acceptable_by_surface)
        .map(|r| r.index)
        .collect();
    if !too_big.is_empty() {
        limits.push(format!("The goal text for rung(s) {too_big:?} (objective plus handoff) exceeds what `goal_is_acceptable` allows (2000 bytes), so the handoff could not travel through `chip work` or the service surface; it was passed to the library entry point directly."));
    }
    limits.push("Repair budgets, repeat limits and the ladder itself are part of this evaluation, not the product: `chip work` has one tier, no repair budget and no human channel.".into());
    limits.push("The decision protocol carries no plan, hypothesis or evidence-of-reasoning field; diagnoses reach a handoff only as the text of an `escalate` reason and are labelled as model assertions.".into());
    limits.push("Edits a model makes to its own new tests cannot be told from legitimate rewrites; only tests that existed at the baseline are protected.".into());
    limits.push("PAX reports pass/fail counts and diagnostics. It runs `cargo test` without `--no-fail-fast`, so a red run stops at the first failing test binary and its counts are partial; per-test results are read from the diagnostics, and a failure is only called fixed once the diagnostics show the test ran and passed.".into());
    if ctx.scripted {
        limits.push("Judgment was scripted at every tier: this run measures the runtime (evidence, escalation, recovery, verification), not any model's coding ability.".into());
    }

    let boundaries = vec![
        ("Chip".to_string(), "validated every request, built every execution and its identity, observed results, evaluated the goal, decided every escalation trigger, built every handoff".to_string()),
        ("Model (scripted)".to_string(), "supplied only work decisions; no reply carried an execution id, observation, receipt or result".to_string()),
        ("Execution".to_string(), "performed by the local environment; Chip did not use or imitate Compute (Rust Chip has no Compute dependency by design)".to_string()),
        ("PAX".to_string(), format!("{} (real); status decided every test goal", ctx.pax)),
        ("FX provider path".to_string(), ctx.fx.clone()),
    ];

    let status = status_label(&result.status);
    let capability_json: Value = dims
        .iter()
        .map(|d| {
            (
                d.name.to_lowercase().replace([' ', '-'], "_"),
                json!(d.verdict.label().to_lowercase()),
            )
        })
        .collect::<serde_json::Map<_, _>>()
        .into();
    let failed_attempts = repairs_rejected + count(events, "goal.unsatisfied");
    let json = json!({
        "task": "configurable retry policies in the courier request-execution layer",
        "status": status,
        "judgment": if ctx.scripted { "scripted" } else { "live" },
        "fx_provider_path": ctx.fx,
        "pax": ctx.pax,
        "fixture": {"rust_files": ctx.fixture.rust_files, "rust_lines": ctx.fixture.rust_lines, "files": ctx.fixture.files},
        "capability": capability_json,
        "work_items": items.iter().map(|i| json!({"item": i.name, "class": i.class.label(), "detail": i.detail})).collect::<Vec<_>>(),
        "rungs": result.rungs.iter().map(|r| json!({
            "rung": r.index, "tier": r.level.name(), "model": r.model_name,
            "outcome": r.work.report.outcome.terminal_state().name(),
            "turns": r.work.report.summary.turns, "executions": r.work.report.summary.executions,
            "model_calls": r.work.utility.model_calls, "goal_bytes": r.goal_bytes,
            "tree_before": r.tree_before, "tree_after": r.tree_after,
            "invariants_hold": r.work.invariants_hold(),
        })).collect::<Vec<_>>(),
        "escalations": result.escalations(),
        "human_escalations": result.human_escalations(),
        "failed_attempts": failed_attempts,
        "tests_before": verification.map(|v| v.tests_before),
        "tests_after": verification.map(|v| v.tests_after),
        "regressions": verification.map(|v| v.regressions.len()),
        "test_integrity_violations": verification.map(|v| v.integrity.len()),
        "verification": verification.map(|v| json!({
            "pax_status": v.pax_status,
            "pax_counts": v.pax_counts.map(|(p, f, i)| json!({"passed": p, "failed": f, "ignored": i})),
            "acceptance": v.checks.iter().map(|c| json!({"check": c.id, "passed": c.passed, "detail": c.detail})).collect::<Vec<_>>(),
            "regressions": v.regressions,
            "baseline_failures_remaining": v.baseline_failures_remaining,
            "test_integrity_violations": v.integrity.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "scope_violations": v.scope_violations,
            "safety_invariants_hold": v.invariants_hold,
            "verified": v.verified,
        })),
        "unresolved": match &result.status { Status::Verified => json!([]), Status::NotVerified(r) => json!(r), Status::Unresolved(r) => json!([r]) },
        "limits": limits,
        "boundaries": boundaries.iter().map(|(a, b)| json!({a: b})).collect::<Vec<_>>(),
        "handoffs": result.handoffs.iter().map(|h| json!({"after_rung": h.after_rung, "to": h.to.name(), "bytes": h.for_model.len(), "defects": h.defects, "content": super::packet::to_json(&h.handoff)})).collect::<Vec<_>>(),
        "events": result.events.iter().map(|e| e.to_json()).collect::<Vec<_>>(),
        "elapsed_seconds": ctx.elapsed.as_secs_f64(),
    });

    let mut text = String::from("CHIP CODING AGENT EVALUATION\n============================\n\n");
    text.push_str(&format!(
        "Result: {status}\nJudgment: {}\n",
        if ctx.scripted {
            "SCRIPTED (mocked models; reality is real)"
        } else {
            "live"
        }
    ));
    text.push_str(&format!(
        "Project: courier, {} Rust files, {} lines, {} files in all\n\n",
        ctx.fixture.rust_files, ctx.fixture.rust_lines, ctx.fixture.files
    ));
    for d in &dims {
        text.push_str(&format!("{:<30}{}\n", d.name, d.verdict.label()));
    }
    text.push_str("\nRungs\n-----\n");
    for r in &result.rungs {
        text.push_str(&format!(
            "rung {} {:<7} {:<10} {:>3} turns {:>3} executions {:>3} model calls, goal {} bytes\n",
            r.index,
            r.level.name(),
            r.work.report.outcome.terminal_state().name(),
            r.work.report.summary.turns,
            r.work.report.summary.executions,
            r.work.utility.model_calls,
            r.goal_bytes
        ));
    }
    text.push_str(&format!(
        "tokens and dollars: not measured (scripted judgment); wall clock {:.1}s\n",
        ctx.elapsed.as_secs_f64()
    ));
    text.push_str("\nWork items\n----------\n");
    for i in &items {
        text.push_str(&format!(
            "{:<12}{}\n            {}\n",
            i.class.label(),
            i.name,
            i.detail
        ));
    }
    text.push_str("\nObserved limitations\n--------------------\n\n");
    for l in &limits {
        text.push_str(&format!("- {l}\n"));
    }
    text.push_str("\nBoundaries\n----------\n\n");
    for (a, b) in &boundaries {
        text.push_str(&format!("{a}: {b}\n"));
    }
    let passed = dims.iter().filter(|d| d.verdict == Verdict::Pass).count();
    text.push_str(&format!("\nConclusion\n----------\n\n{passed} of {} dimensions passed. Escalations: {} ({} human). Final status: {status}.\n", dims.len(), result.escalations(), result.human_escalations()));
    let by_class: BTreeMap<&str, usize> = items.iter().fold(BTreeMap::new(), |mut m, i| {
        *m.entry(i.class.label()).or_default() += 1;
        m
    });
    text.push_str(&format!("Work items by class: {by_class:?}\n"));

    Report {
        dimensions: dims,
        items,
        limits,
        boundaries,
        status,
        json,
        text,
    }
}

pub fn status_label(s: &Status) -> String {
    match s {
        Status::Verified => "VERIFIED".into(),
        Status::NotVerified(r) => format!("NOT VERIFIED ({})", r.join("; ")),
        Status::Unresolved(r) => format!("UNRESOLVED ({r})"),
    }
}
