//! Chip's own, deterministic decisions about when a rung of work must stop and escalate.
//!
//! These are `LocalWorkPolicy` implementations: they see only the recorded observations and
//! decisions, never model prose, and what they decide is Chip's. They bound repair work:
//!
//! * a **repair budget**: how many times the tests may stay red after a change made to repair a
//!   known failure;
//! * a **repeat limit**: the same request, unchanged, over and over.
//!
//! The product's own `CompleteWhenVerified` always has the first word: work whose tests passed
//! after the last change completes, whatever else is true.

use chip_core::{CapabilityRequest, LocalWorkPolicy, Observation, WorkDecision, WorkView};
use chip_pax::{PaxStatus, parse_execution_result};
use chip_project::write_summary;

pub use chip_cli::software_work::CompleteWhenVerified;

#[derive(Debug, Clone, Copy)]
pub struct LadderPolicy {
    /// Failed repairs tolerated before escalating.
    pub repair_budget: u32,
    /// Identical consecutive requests tolerated before escalating.
    pub max_repeats: u32,
}

impl LocalWorkPolicy for LadderPolicy {
    fn propose(&self, view: &WorkView<'_>) -> Option<WorkDecision> {
        if let Some(done) = CompleteWhenVerified.propose(view) {
            return Some(done);
        }
        let failed = failed_repairs(view.observations);
        if failed >= self.repair_budget {
            return Some(WorkDecision::Escalate {
                reason: format!(
                    "repair budget exhausted: the tests stayed red after {failed} changes made to repair a known failure (budget {})",
                    self.repair_budget
                ),
            });
        }
        let repeats = trailing_repeats(view);
        if repeats >= self.max_repeats {
            return Some(WorkDecision::Escalate {
                reason: format!(
                    "stuck: the same request was made {repeats} times in a row (limit {})",
                    self.max_repeats
                ),
            });
        }
        None
    }
}

/// PAX's status for an observation, if it is a `pax.test` result.
pub fn pax_status(o: &Observation) -> Option<PaxStatus> {
    let first = o.output.as_deref()?.lines().next()?;
    parse_execution_result(first.as_bytes())
        .ok()
        .map(|r| r.status)
}

/// How many times the tests stayed red after a change made while a failure was already known.
pub fn failed_repairs(observations: &[Observation]) -> u32 {
    let (mut known_failure, mut changed, mut failed) = (false, false, 0);
    for o in observations {
        if write_summary(o).is_some_and(|(_, c)| c) {
            changed = true;
        }
        match pax_status(o) {
            Some(PaxStatus::Passed) => {
                known_failure = false;
                changed = false;
            }
            Some(_) => {
                if known_failure && changed {
                    failed += 1;
                }
                known_failure = true;
                changed = false;
            }
            None => {}
        }
    }
    failed
}

fn same_request(a: &CapabilityRequest, b: &CapabilityRequest) -> bool {
    a.capability_id == b.capability_id && a.inputs == b.inputs
}

/// Length of the run of identical requests that ends the decision history.
pub fn trailing_repeats(view: &WorkView<'_>) -> u32 {
    let requests: Vec<&CapabilityRequest> = view
        .decisions
        .iter()
        .filter_map(|d| match &d.decision {
            WorkDecision::RequestCapability(r) => Some(r),
            _ => None,
        })
        .collect();
    let Some(last) = requests.last() else {
        return 0;
    };
    requests
        .iter()
        .rev()
        .take_while(|r| same_request(r, last))
        .count() as u32
}
