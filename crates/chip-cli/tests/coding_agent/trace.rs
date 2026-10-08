//! The evidence trail of a run, derived from the Chip event stream and its observations.
//!
//! Nothing here is a second event system: every entry is computed from `WorkEvent`s and the
//! `Observation`s they refer to, or (for the few moments that happen between runs) recorded by
//! the ladder and marked as such. Each entry says who is responsible for it:
//!
//! * `environment`: reality, as observed by the local environment or PAX;
//! * `chip`: a decision or derivation the runtime made;
//! * `ladder`: a moment between two runs, recorded by the evaluation harness;
//! * `model-asserted`: something a model said. Kept, labelled, never promoted to evidence.

use std::collections::BTreeSet;

use chip_core::{DecisionSource, WorkDecision, WorkEvent, WorkOutcome, WorkReport};
use chip_pax::{PaxStatus, TestCounts, parse_execution_result};
use chip_project::write_summary;
use serde_json::json;

#[derive(Debug, Clone, PartialEq)]
pub struct TraceEvent {
    pub rung: usize,
    pub turn: usize,
    pub name: String,
    pub authority: &'static str,
    pub detail: serde_json::Value,
}

impl TraceEvent {
    pub fn to_json(&self) -> serde_json::Value {
        json!({"rung": self.rung, "turn": self.turn, "event": self.name,
               "authority": self.authority, "detail": self.detail})
    }
}

/// One `pax.test` result, as Chip recorded it.
#[derive(Debug, Clone, PartialEq)]
pub struct TestRun {
    pub rung: usize,
    pub status: PaxStatus,
    pub counts: Option<TestCounts>,
    /// Names parsed from the native tool's diagnostics. Diagnostic only: PAX's `status` decides.
    pub failed_tests: Vec<String>,
    /// Every `test NAME ... STATUS` line in the diagnostics (name to `ok` / `FAILED` / `ignored`).
    /// Diagnostic only. Cargo stops at the first failing test binary, so a red run does not list
    /// the tests in binaries that never ran.
    pub results: std::collections::BTreeMap<String, String>,
}

impl TestRun {
    pub fn passed(&self) -> bool {
        self.status == PaxStatus::Passed
    }

    /// The diagnostics show this test ran and passed.
    pub fn ran_ok(&self, name: &str) -> bool {
        self.results.get(name).is_some_and(|s| s == "ok")
    }
}

pub fn test_run(rung: usize, output: &str) -> Option<TestRun> {
    let first = output.lines().next()?;
    let result = parse_execution_result(first.as_bytes()).ok()?;
    let mut results = std::collections::BTreeMap::new();
    for line in output.lines() {
        if let Some((name, status)) = line
            .strip_prefix("test ")
            .and_then(|r| r.rsplit_once(" ... "))
        {
            let status = status.split_whitespace().next().unwrap_or("?").to_string();
            // Two binaries can have a test of the same name; a failure is never overwritten.
            let entry = results
                .entry(name.to_string())
                .or_insert_with(|| status.clone());
            if status == "FAILED" {
                *entry = status;
            }
        }
    }
    let failed_tests: Vec<String> = output
        .lines()
        .filter_map(|l| {
            l.strip_prefix("---- ")
                .and_then(|r| r.strip_suffix(" stdout ----"))
        })
        .map(str::to_string)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    Some(TestRun {
        rung,
        status: result.status,
        counts: result.tests,
        failed_tests,
        results,
    })
}

fn input_text(r: &chip_core::CapabilityRequest, name: &str) -> Option<String> {
    match r.inputs.get(name)? {
        chip_core::InputValue::Text(t) => Some(t.clone()),
        _ => None,
    }
}

/// The trail of one run. `rung` is the run's position in the ladder.
pub fn derive(rung: usize, report: &WorkReport) -> (Vec<TraceEvent>, Vec<TestRun>) {
    let mut out = Vec::new();
    let mut runs: Vec<TestRun> = Vec::new();
    let requests: Vec<&chip_core::CapabilityRequest> = report
        .decisions
        .iter()
        .filter_map(|d| match &d.decision {
            WorkDecision::RequestCapability(r) => Some(r),
            _ => None,
        })
        .collect();
    let (mut req_i, mut obs_i) = (0usize, 0usize);
    let mut inspected = false;
    let mut failure_open = false;
    let mut repair_pending = false;
    let mut last_failed: Option<usize> = None;
    let mut push = |turn: usize, name: &str, authority: &'static str, detail: serde_json::Value| {
        out.push(TraceEvent {
            rung,
            turn,
            name: name.to_string(),
            authority,
            detail,
        });
    };
    for event in &report.events {
        match event {
            WorkEvent::WorkStarted { goal, limits, .. } => push(
                0,
                "task.started",
                "chip",
                json!({"goal_bytes": goal.len(), "max_turns": limits.max_turns,
                       "max_executions": limits.max_executions}),
            ),
            WorkEvent::LocalDecision { turn, decision, .. } => push(
                *turn,
                "decision.local",
                "chip",
                json!({"decision": decision}),
            ),
            WorkEvent::ModelCalled {
                turn, succeeded, ..
            } => push(
                *turn,
                "model.called",
                "chip",
                json!({"succeeded": succeeded}),
            ),
            WorkEvent::CapabilityRequested {
                turn, capability, ..
            } => {
                let request = requests.get(req_i).copied();
                req_i += 1;
                let id = capability.as_str();
                let path = request.and_then(|r| input_text(r, "path"));
                match id {
                    "project.list" | "project.search" if !inspected || id == "project.search" => {
                        inspected = true;
                        let q = request
                            .and_then(|r| input_text(r, "query").or_else(|| input_text(r, "path")));
                        push(
                            *turn,
                            "repository.inspected",
                            "environment",
                            json!({"via": id, "of": q}),
                        );
                    }
                    "project.read" => {
                        push(*turn, "file.read", "environment", json!({"path": path}));
                    }
                    "pax.test" => {
                        push(*turn, "test.started", "chip", json!({"capability": id}));
                    }
                    _ => {}
                }
            }
            WorkEvent::ObservationRecorded { turn, .. } => {
                let Some(o) = report.observations.get(obs_i) else {
                    continue;
                };
                obs_i += 1;
                if let Some((path, changed)) = write_summary(o) {
                    if changed {
                        push(*turn, "file.changed", "environment", json!({"path": path}));
                        if failure_open {
                            repair_pending = true;
                            push(*turn, "repair.attempted", "chip", json!({"path": path}));
                        }
                    } else {
                        push(
                            *turn,
                            "file.write_unchanged",
                            "environment",
                            json!({"path": path}),
                        );
                    }
                    continue;
                }
                let Some(text) = o.output.as_deref() else {
                    continue;
                };
                let Some(run) = test_run(rung, text) else {
                    continue;
                };
                let failed_count = run.counts.map_or(0, |c| c.failed as usize);
                push(
                    *turn,
                    "test.completed",
                    "environment",
                    json!({"status": run.status.as_str(),
                           "passed": run.counts.map(|c| c.passed), "failed": run.counts.map(|c| c.failed),
                           "ignored": run.counts.map(|c| c.ignored),
                           "failed_tests": run.failed_tests}),
                );
                if run.passed() {
                    if failure_open {
                        push(
                            *turn,
                            "repair.accepted",
                            "chip",
                            json!({"basis": "pax status passed"}),
                        );
                    }
                    failure_open = false;
                    repair_pending = false;
                    last_failed = None;
                } else {
                    push(
                        *turn,
                        "failure.observed",
                        "environment",
                        json!({"status": run.status.as_str(), "failed_tests": run.failed_tests}),
                    );
                    if repair_pending {
                        let better = last_failed.is_some_and(|p| failed_count < p);
                        push(
                            *turn,
                            if better {
                                "repair.accepted"
                            } else {
                                "repair.rejected"
                            },
                            "chip",
                            json!({"basis": "failed-test count", "before": last_failed, "after": failed_count}),
                        );
                    }
                    failure_open = true;
                    repair_pending = false;
                    last_failed = Some(failed_count);
                }
                runs.push(run);
            }
            WorkEvent::GoalEvaluated {
                turn, remaining, ..
            } => push(
                *turn,
                "goal.evaluated",
                "chip",
                json!({"satisfied": *remaining == 0, "remaining": remaining}),
            ),
            WorkEvent::WorkEscalated { reason, .. } => {
                let local = report
                    .decisions
                    .last()
                    .is_some_and(|d| d.source == DecisionSource::Local);
                push(
                    report.summary.turns,
                    "escalation.requested",
                    if local { "chip" } else { "model-asserted" },
                    json!({"source": if local { "local-policy" } else { "model" }, "reason": reason}),
                );
                if !local {
                    push(
                        report.summary.turns,
                        "diagnosis.asserted",
                        "model-asserted",
                        json!({"text": reason, "evidence": false}),
                    );
                }
            }
            WorkEvent::WorkCompleted { .. } => push(
                report.summary.turns,
                "task.completed",
                "chip",
                json!({"runtime_completion": true}),
            ),
            WorkEvent::WorkBlocked { reason, .. } => push(
                report.summary.turns,
                "task.blocked",
                "chip",
                json!({"reason": reason}),
            ),
            WorkEvent::WorkLimitReached { limit, .. } => push(
                report.summary.turns,
                "task.limit_reached",
                "chip",
                json!({"limit": limit.name()}),
            ),
            WorkEvent::WorkFailed { reason, .. } => push(
                report.summary.turns,
                "task.failed",
                "chip",
                json!({"reason": reason}),
            ),
            _ => {}
        }
    }
    let _ = matches!(report.outcome, WorkOutcome::Completed { .. });
    (out, runs)
}

pub fn count(trace: &[TraceEvent], name: &str) -> usize {
    trace.iter().filter(|e| e.name == name).count()
}
