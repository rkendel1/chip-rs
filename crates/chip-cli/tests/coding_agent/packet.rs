//! What an escalation carries to the next intelligence.
//!
//! A [`Handoff`] is built by Chip from the recorded trajectory and from the project as it is on
//! disk. Every fact in it comes from an observation or from a comparison of files. What a model
//! *claimed* (a hypothesis, a diagnosis, the question it wants answered) is kept in its own
//! sections and labelled as an assertion: it is context for the reader, never evidence.
//!
//! Two renderings: a compact one that becomes part of the next rung's goal, and the human
//! escalation packet. A handoff is also *assessed*: if it would not give the reader enough to act
//! on, Chip refuses to escalate with it.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::json;

use super::ladder::{Level, RungRecord};
use super::trace::{TestRun, TraceEvent, count};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// The same tier continues, told what the acceptance gate found missing.
    Continue,
    Strong,
    Human,
}

impl Target {
    pub fn name(self) -> &'static str {
        match self {
            Target::Continue => "same tier (continue)",
            Target::Strong => "stronger model",
            Target::Human => "human",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Handoff {
    pub objective: String,
    pub target: Target,
    /// Why Chip stopped the last rung, in Chip's words.
    pub stop_reason: String,
    pub rungs: Vec<String>,
    pub files_changed: Vec<String>,
    pub tests: Vec<TestRun>,
    pub attempted: Vec<String>,
    pub successful: Vec<String>,
    pub failed: Vec<String>,
    pub relevant_files: Vec<String>,
    pub prior_successes: Vec<String>,
    pub prior_failures: Vec<String>,
    pub unresolved: Vec<String>,
    pub not_known: Vec<String>,
    /// Model assertions, in order, with the rung that made them. Not evidence.
    pub hypotheses: Vec<String>,
    pub decision_needed: Option<String>,
    pub human_decisions: Vec<String>,
}

fn changed_files(
    baseline: &BTreeMap<String, String>,
    now: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut out: Vec<String> = now
        .iter()
        .filter(|(k, v)| baseline.get(*k) != Some(*v))
        .map(|(k, _)| k.clone())
        .collect();
    out.extend(
        baseline
            .keys()
            .filter(|k| !now.contains_key(*k))
            .map(|k| format!("{k} (deleted)")),
    );
    out
}

fn str_of(e: &TraceEvent, key: &str) -> String {
    e.detail[key].as_str().unwrap_or("?").to_string()
}

/// Builds a handoff from everything that has happened so far.
pub fn build(
    objective: &str,
    target: Target,
    rungs: &[RungRecord],
    baseline: &BTreeMap<String, String>,
    now: &BTreeMap<String, String>,
    unmet: &[String],
    human_decisions: &[String],
) -> Handoff {
    let mut attempted = Vec::new();
    let mut successful = Vec::new();
    let mut failed = Vec::new();
    let mut relevant: Vec<String> = Vec::new();
    let mut hypotheses = Vec::new();
    let mut tests: Vec<TestRun> = Vec::new();
    let mut rung_lines = Vec::new();
    let mut decision_needed = None;
    let mut stop_reason = String::new();
    let mut prior_failures = Vec::new();

    for r in rungs {
        let outcome = match &r.work.report.outcome {
            chip_core::WorkOutcome::Completed { .. } => "completed (runtime)".to_string(),
            chip_core::WorkOutcome::Escalated { reason } => format!("escalated: {reason}"),
            chip_core::WorkOutcome::Blocked { reason } => format!("blocked: {reason}"),
            chip_core::WorkOutcome::LimitReached { limit } => {
                format!("limit reached: {}", limit.name())
            }
            chip_core::WorkOutcome::Failed { reason } => format!("failed: {reason}"),
        };
        rung_lines.push(format!(
            "rung {} [{} tier]: {} turns, {} executions, {}",
            r.index,
            r.level.name(),
            r.work.report.summary.turns,
            r.work.report.summary.executions,
            outcome
        ));
        stop_reason = outcome;
        for e in &r.trace {
            let tag = format!("r{}", r.index);
            match e.name.as_str() {
                "repository.inspected" => {
                    attempted.push(format!(
                        "{tag} inspected the repository ({})",
                        str_of(e, "via")
                    ));
                    successful.push(format!("{tag} inspected the repository"));
                }
                "file.read" => {
                    let p = str_of(e, "path");
                    attempted.push(format!("{tag} read {p}"));
                    if !relevant.contains(&p) {
                        relevant.push(p);
                    }
                }
                "file.changed" => {
                    let p = str_of(e, "path");
                    attempted.push(format!("{tag} changed {p}"));
                    successful.push(format!("{tag} changed {p}"));
                    if !relevant.contains(&p) {
                        relevant.push(p);
                    }
                }
                "test.completed" => {
                    let line = format!(
                        "{tag} tests: {} passed, {} failed ({}){}",
                        e.detail["passed"],
                        e.detail["failed"],
                        str_of(e, "status"),
                        match e.detail["failed_tests"].as_array() {
                            Some(a) if !a.is_empty() => format!(
                                "; failing: {}",
                                a.iter()
                                    .filter_map(|x| x.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                            _ => String::new(),
                        }
                    );
                    attempted.push(line.clone());
                    if e.detail["status"] == "passed" {
                        successful.push(line);
                    } else {
                        failed.push(line);
                    }
                }
                "repair.rejected" => {
                    let l = format!(
                        "{tag} a change made to repair a failure did not fix it (failed tests: {} -> {})",
                        e.detail["before"], e.detail["after"]
                    );
                    failed.push(l.clone());
                    prior_failures.push(l);
                }
                "repair.accepted" => successful.push(format!(
                    "{tag} a repair was accepted ({})",
                    str_of(e, "basis")
                )),
                "diagnosis.asserted" => {
                    let t = str_of(e, "text");
                    hypotheses.push(format!("{tag} ({} tier) asserted: {t}", r.level.name()));
                    decision_needed = Some(t);
                }
                _ => {}
            }
        }
        tests.extend(r.tests.clone());
    }

    let files_changed = changed_files(baseline, now);
    let last = tests.last();
    let mut prior_successes: Vec<String> = Vec::new();
    if let Some(first) = tests.iter().position(|t| !t.passed()) {
        for name in &tests[first].failed_tests {
            if let Some(later) = tests[first + 1..].iter().find(|t| t.ran_ok(name)) {
                prior_successes.push(format!(
                    "test `{name}` failed in rung {} and ran and passed in rung {}",
                    tests[first].rung, later.rung
                ));
            }
        }
    }
    if last.is_some_and(|t| t.passed()) {
        prior_successes.push("the latest test run passed".into());
    }

    let mut unresolved: Vec<String> = Vec::new();
    if let Some(t) = last {
        for n in &t.failed_tests {
            unresolved.push(format!("test `{n}` still fails"));
        }
        if !t.passed() && t.failed_tests.is_empty() {
            unresolved.push(format!(
                "the latest test run is `{}` and names no failing test",
                t.status.as_str()
            ));
        }
    }
    unresolved.extend(unmet.iter().map(|u| format!("acceptance not met: {u}")));

    let mut not_known = Vec::new();
    if !hypotheses.is_empty() && last.is_some_and(|t| !t.passed()) {
        not_known.push(
            "which of the asserted explanations is correct: no observation distinguishes them"
                .to_string(),
        );
    }
    // A failure with no later run showing it ran and passed is not known to be fixed, and a later
    // red run may simply have stopped before its binary.
    let mut seen = BTreeSet::new();
    for (i, run) in tests.iter().enumerate() {
        for name in &run.failed_tests {
            if !seen.insert(name.clone()) || tests[i + 1..].iter().any(|t| t.ran_ok(name)) {
                continue;
            }
            let still = last.is_some_and(|t| t.failed_tests.contains(name));
            let failing_runs = tests
                .iter()
                .filter(|t| t.failed_tests.contains(name))
                .count();
            if still && failing_runs >= 2 {
                not_known.push(format!(
                    "why `{name}` still fails after the changes already tried"
                ));
            } else if !still {
                not_known.push(format!(
                    "whether `{name}` (failing in rung {}) is fixed: no later run shows it passing; later red runs stopped at an earlier failing test binary",
                    run.rung
                ));
            }
        }
    }
    if last.is_some_and(|t| !t.passed()) {
        not_known.push("whether the tests in binaries after the failing one pass: Cargo stops at the first failing test binary, so the counts above are not the size of the suite".to_string());
    }
    not_known.push("PAX reports pass/fail counts and the native tool's diagnostics; failing test names above are read from those diagnostics".to_string());

    Handoff {
        objective: objective.to_string(),
        target,
        stop_reason,
        rungs: rung_lines,
        files_changed,
        tests,
        attempted,
        successful,
        failed,
        relevant_files: relevant,
        prior_successes,
        prior_failures,
        unresolved,
        not_known,
        hypotheses,
        decision_needed: if target == Target::Human {
            decision_needed
        } else {
            None
        },
        human_decisions: human_decisions.to_vec(),
    }
}

/// Reasons this handoff is not good enough to escalate with. Empty means it is.
pub fn assess(h: &Handoff, rungs: &[RungRecord]) -> Vec<String> {
    let mut defects = Vec::new();
    if h.objective.trim().is_empty() {
        defects.push("no objective".to_string());
    }
    if rungs.is_empty() || rungs.iter().all(|r| r.work.report.observations.is_empty()) {
        defects.push(
            "no observations: nothing was executed, so there is no evidence to hand over".into(),
        );
    }
    if h.tests.is_empty() {
        defects.push("no test result: the reader cannot tell what state the project is in".into());
    }
    if h.attempted.is_empty() {
        defects.push("no record of what was attempted".into());
    }
    if h.tests.last().is_some_and(|t| !t.passed()) && h.unresolved.is_empty() {
        defects.push("the tests are red but no unresolved item is stated".into());
    }
    // Cross-check: every changed file was changed through an observed write; the handoff must not
    // describe a project that something other than Chip's own writes altered.
    let written: BTreeSet<String> = rungs
        .iter()
        .flat_map(|r| r.trace.iter())
        .filter(|e| e.name == "file.changed")
        .map(|e| str_of(e, "path"))
        .collect();
    for f in &h.files_changed {
        if !written.contains(f) {
            defects.push(format!(
                "{f} differs from the baseline but no observed write changed it"
            ));
        }
    }
    // Cross-check: the test lines are exactly what the observations say.
    let from_trace: usize = rungs
        .iter()
        .map(|r| count(&r.trace, "test.completed"))
        .sum();
    if from_trace != h.tests.len() {
        defects.push(format!(
            "{} test results in the trail but {} in the handoff",
            from_trace,
            h.tests.len()
        ));
    }
    if h.target == Target::Human && h.decision_needed.is_none() {
        defects.push("a human escalation must state the decision required".into());
    }
    defects
}

fn cap<T: AsRef<str>>(items: &[T], n: usize) -> Vec<String> {
    let mut v: Vec<String> = items
        .iter()
        .take(n)
        .map(|s| s.as_ref().to_string())
        .collect();
    if items.len() > n {
        v.push(format!("(+{} more)", items.len() - n));
    }
    v
}

fn bullets(items: &[String]) -> String {
    if items.is_empty() {
        "  (none)\n".to_string()
    } else {
        items.iter().map(|i| format!("  - {i}\n")).collect()
    }
}

/// The compact form that travels in the next rung's goal.
pub fn render_for_model(h: &Handoff) -> String {
    let mut s = format!(
        "CODING HANDOFF (built by Chip from observed events; to: {})\nStopped because: {}\n",
        h.target.name(),
        h.stop_reason
    );
    s.push_str(&format!("Work so far:\n{}", bullets(&cap(&h.rungs, 8))));
    s.push_str(&format!(
        "Files changed (project vs baseline):\n{}",
        bullets(&cap(&h.files_changed, 16))
    ));
    s.push_str("Test results:\n");
    let recent: Vec<String> = h
        .tests
        .iter()
        .map(|t| {
            format!(
                "r{} {} (passed {:?} failed {:?}{}){}",
                t.rung,
                t.status.as_str(),
                t.counts.map(|c| c.passed),
                t.counts.map(|c| c.failed),
                if t.passed() {
                    ""
                } else {
                    "; partial: stopped at the first failing binary"
                },
                if t.failed_tests.is_empty() {
                    String::new()
                } else {
                    format!(" failing: {}", t.failed_tests.join(", "))
                }
            )
        })
        .collect();
    s.push_str(&bullets(&cap(&recent, 8)));
    s.push_str(&format!("Unresolved:\n{}", bullets(&cap(&h.unresolved, 8))));
    s.push_str(&format!(
        "Already succeeded:\n{}",
        bullets(&cap(&h.prior_successes, 6))
    ));
    s.push_str(&format!(
        "Already failed:\n{}",
        bullets(&cap(&h.prior_failures, 6))
    ));
    s.push_str(&format!(
        "Files already read or changed:\n{}",
        bullets(&cap(&h.relevant_files, 24))
    ));
    s.push_str(&format!(
        "Asserted by a model (not evidence):\n{}",
        bullets(&cap(&h.hypotheses, 4))
    ));
    s.push_str(&format!("Not known:\n{}", bullets(&cap(&h.not_known, 4))));
    s.push_str(&format!(
        "Decisions already given by a person:\n{}",
        bullets(&h.human_decisions)
    ));
    s
}

/// The packet a person reads.
pub fn render_for_human(h: &Handoff, project: &str) -> String {
    let last = h.tests.last();
    let state = match last {
        Some(t) => format!(
            "{} files differ from the baseline. In the latest run {} tests passed and {} failed (PAX status: {}).{}",
            h.files_changed.len(),
            t.counts.map_or("?".to_string(), |c| c.passed.to_string()),
            t.counts.map_or("?".to_string(), |c| c.failed.to_string()),
            t.status.as_str(),
            if t.passed() {
                ""
            } else {
                " The run stopped at the first failing test binary, so these counts are not the size of the suite."
            }
        ),
        None => "no tests were run".to_string(),
    };
    let numbered = |items: &[String]| -> String {
        if items.is_empty() {
            return "  (none)\n".into();
        }
        items
            .iter()
            .enumerate()
            .map(|(i, l)| format!("  {}. {l}\n", i + 1))
            .collect()
    };
    let mut s = String::from("CODING ESCALATION\n\n");
    s.push_str(&format!(
        "Objective\n---------\n\n{}\n\nProject: {project}\n\n",
        h.objective
    ));
    s.push_str(&format!(
        "Why Chip stopped\n----------------\n\n{}\n\n",
        h.stop_reason
    ));
    s.push_str(&format!("Current state\n-------------\n\n{state}\n\n"));
    s.push_str("What was attempted\n------------------\n\n");
    s.push_str(&numbered(&cap(&h.attempted, 40)));
    s.push_str("\nEvidence\n--------\n\n");
    for t in &h.tests {
        s.push_str(&format!(
            "  pax.test (rung {}) -> {}: {} passed, {} failed\n",
            t.rung,
            t.status.as_str(),
            t.counts.map_or("?".to_string(), |c| c.passed.to_string()),
            t.counts.map_or("?".to_string(), |c| c.failed.to_string())
        ));
    }
    s.push_str("\nFailure\n-------\n\n");
    s.push_str(&bullets(&h.unresolved));
    s.push_str("\nWhat we know (observed)\n-----------------------\n\n");
    s.push_str(&bullets(&cap(&h.prior_successes, 6)));
    s.push_str(&bullets(&cap(&h.prior_failures, 6)));
    s.push_str("\nWhat we do not know\n-------------------\n\n");
    s.push_str(&bullets(&h.not_known));
    s.push_str("\nHypotheses (asserted by a model; not verified)\n----------------------------------------------\n\n");
    s.push_str(&numbered(&cap(&h.hypotheses, 6)));
    s.push_str("\nRecommended next investigation\n------------------------------\n\n");
    let mut next = Vec::new();
    if let Some(t) = last {
        for n in &t.failed_tests {
            next.push(format!("Read the failing test `{n}` and the code it exercises, then decide which expectation is authoritative."));
        }
    }
    if let Some(f) = h.relevant_files.last() {
        next.push(format!("The most recently touched file is {f}."));
    }
    s.push_str(&numbered(&next));
    s.push_str("\nFiles currently modified\n------------------------\n\n");
    s.push_str(&bullets(&h.files_changed));
    s.push_str("\nHuman decision required\n-----------------------\n\n");
    s.push_str(&format!(
        "{}\n\n(The question above was stated by a model; Chip has not checked it.)\n",
        h.decision_needed.as_deref().unwrap_or("(none stated)")
    ));
    s
}

pub fn to_json(h: &Handoff) -> serde_json::Value {
    json!({
        "objective": h.objective,
        "to": h.target.name(),
        "stop_reason": h.stop_reason,
        "rungs": h.rungs,
        "repository_state": {"files_changed": h.files_changed},
        "test_results": h.tests.iter().map(|t| json!({
            "rung": t.rung, "status": t.status.as_str(),
            "passed": t.counts.map(|c| c.passed), "failed": t.counts.map(|c| c.failed),
            "failed_tests": t.failed_tests})).collect::<Vec<_>>(),
        "attempted_actions": h.attempted, "successful_actions": h.successful, "failed_actions": h.failed,
        "relevant_files": h.relevant_files,
        "prior_successes": h.prior_successes, "prior_failures": h.prior_failures,
        "unresolved_questions": h.unresolved, "not_known": h.not_known,
        "hypotheses_asserted_by_model": h.hypotheses,
        "decision_needed": h.decision_needed,
    })
}

impl Level {
    pub fn name(self) -> &'static str {
        match self {
            Level::Cheap => "cheap",
            Level::Strong => "strong",
        }
    }
}
