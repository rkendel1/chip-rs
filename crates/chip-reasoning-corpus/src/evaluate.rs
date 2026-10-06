//! Replay harness. It hands each case's structured input to a reasoner and
//! records the normalized verdict. It has no access to an agent, model, executor
//! or evidence store, so it cannot cause any of them to be used.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use chip_core::LocalReasoner;

use crate::corpus::{Category, ReasoningCase, Verdict};

/// The result for one case. Only the normalized verdict is kept, never reply text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseResult {
    pub case_id: &'static str,
    pub category: Category,
    pub expected: Verdict,
    /// `None` if the reasoner failed to produce a verdict.
    pub actual: Option<Verdict>,
    pub correct: bool,
    pub latency: Duration,
    /// Short error text when the reasoner failed.
    pub error: Option<String>,
}

impl CaseResult {
    /// Predicted CONTINUE where ESCALATE was required: the unsafe error.
    pub fn is_false_continue(&self) -> bool {
        self.expected == Verdict::Escalate && self.actual == Some(Verdict::Continue)
    }

    /// Predicted ESCALATE where CONTINUE was correct: safe but costly.
    pub fn is_needless_escalation(&self) -> bool {
        self.expected == Verdict::Continue && self.actual == Some(Verdict::Escalate)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvaluationResult {
    pub total: usize,
    pub correct: usize,
    pub incorrect: usize,
    pub continue_count: usize,
    pub escalate_count: usize,
    pub reasoner_calls: usize,
    pub cases: Vec<CaseResult>,
}

impl EvaluationResult {
    pub fn accuracy(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.correct as f64 / self.total as f64
        }
    }

    pub fn mismatches(&self) -> impl Iterator<Item = &CaseResult> {
        self.cases.iter().filter(|c| !c.correct)
    }

    pub fn false_continues(&self) -> usize {
        self.cases.iter().filter(|c| c.is_false_continue()).count()
    }

    pub fn needless_escalations(&self) -> usize {
        self.cases
            .iter()
            .filter(|c| c.is_needless_escalation())
            .count()
    }

    pub fn median_latency(&self) -> Duration {
        let mut all: Vec<Duration> = self.cases.iter().map(|c| c.latency).collect();
        all.sort();
        all.get(all.len() / 2).copied().unwrap_or_default()
    }

    /// (correct, total) for each category present.
    pub fn by_category(&self) -> BTreeMap<&'static str, (usize, usize)> {
        let mut map = BTreeMap::new();
        for c in &self.cases {
            let entry = map.entry(c.category.name()).or_insert((0, 0));
            entry.1 += 1;
            entry.0 += usize::from(c.correct);
        }
        map
    }
}

/// Replays `corpus` through `reasoner`, one call per case.
pub fn evaluate(reasoner: &dyn LocalReasoner, corpus: &[ReasoningCase]) -> EvaluationResult {
    let mut cases = Vec::with_capacity(corpus.len());
    for case in corpus {
        let started = Instant::now();
        let outcome = reasoner.reason(&case.input);
        let latency = started.elapsed();
        let (actual, error) = match outcome {
            Ok(result) => (Some(Verdict::of(&result)), None),
            Err(error) => (None, Some(error.to_string())),
        };
        cases.push(CaseResult {
            case_id: case.id,
            category: case.category,
            expected: case.expected,
            correct: actual == Some(case.expected),
            actual,
            latency,
            error,
        });
    }
    EvaluationResult {
        total: cases.len(),
        correct: cases.iter().filter(|c| c.correct).count(),
        incorrect: cases.iter().filter(|c| !c.correct).count(),
        continue_count: cases
            .iter()
            .filter(|c| c.actual == Some(Verdict::Continue))
            .count(),
        escalate_count: cases
            .iter()
            .filter(|c| c.actual == Some(Verdict::Escalate))
            .count(),
        reasoner_calls: cases.len(),
        cases,
    }
}

/// A case on which two reasoners returned different verdicts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disagreement {
    pub case_id: &'static str,
    pub first: Option<Verdict>,
    pub second: Option<Verdict>,
}

/// Cases where two evaluations of the same corpus differ.
pub fn agreement(first: &EvaluationResult, second: &EvaluationResult) -> Vec<Disagreement> {
    first
        .cases
        .iter()
        .zip(&second.cases)
        .filter(|(a, b)| a.actual != b.actual)
        .map(|(a, b)| Disagreement {
            case_id: a.case_id,
            first: a.actual,
            second: b.actual,
        })
        .collect()
}
