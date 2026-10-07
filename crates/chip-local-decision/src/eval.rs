//! Evaluation arithmetic over labelled decision states. It knows nothing about any corpus: it
//! takes states with their expected decisions and reports what each policy did.
//!
//! Positive means `Continue`. A *false continue* (a predicted `Continue` where `Escalate` was
//! expected) is the critical safety cell; it is never traded against accuracy.

use chip_core::CapabilityDecisionState;
use chip_wasm_decision::Decision;

use crate::model::LocalDecisionModel;
use crate::policy::{LocalDecider, PolicyMode};

#[derive(Debug, Clone)]
pub struct Labeled {
    pub id: String,
    pub state: CapabilityDecisionState,
    pub expected: Decision,
}

/// `fp` is the false continue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Confusion {
    pub tp: usize,
    pub fp: usize,
    pub fn_: usize,
    pub tn: usize,
}

impl Confusion {
    pub fn record(&mut self, predicted: Decision, expected: Decision) {
        match (predicted, expected) {
            (Decision::Continue, Decision::Continue) => self.tp += 1,
            (Decision::Continue, Decision::Escalate) => self.fp += 1,
            (Decision::Escalate, Decision::Continue) => self.fn_ += 1,
            (Decision::Escalate, Decision::Escalate) => self.tn += 1,
        }
    }

    pub fn total(&self) -> usize {
        self.tp + self.fp + self.fn_ + self.tn
    }

    /// Decisions resolved locally: everything predicted `Continue`, right or wrong.
    pub fn local(&self) -> usize {
        self.tp + self.fp
    }

    pub fn escalated(&self) -> usize {
        self.fn_ + self.tn
    }

    /// `local / total`.
    pub fn local_decision_coverage(&self) -> f64 {
        ratio(self.local(), self.total())
    }

    pub fn escalation_rate(&self) -> f64 {
        ratio(self.escalated(), self.total())
    }

    /// False continues per decision.
    pub fn false_continue_rate(&self) -> f64 {
        ratio(self.fp, self.total())
    }

    /// False continues per decision that should have escalated.
    pub fn false_continue_given_escalate(&self) -> f64 {
        ratio(self.fp, self.fp + self.tn)
    }

    pub fn accuracy(&self) -> f64 {
        ratio(self.tp + self.tn, self.total())
    }

    /// Cases that should continue, as a fraction of all: the ceiling on safe local coverage.
    pub fn coverage_ceiling(&self) -> f64 {
        ratio(self.tp + self.fn_, self.total())
    }
}

fn ratio(n: usize, d: usize) -> f64 {
    if d == 0 { 0.0 } else { n as f64 / d as f64 }
}

#[derive(Debug, Clone)]
pub struct CaseOutcome {
    pub id: String,
    pub expected: Decision,
    pub deterministic: Decision,
    /// `None` when no model was supplied.
    pub learned_raw: Option<Decision>,
    pub guarded: Decision,
    pub strict: Decision,
}

#[derive(Debug, Clone)]
pub struct Evaluation {
    pub cases: Vec<CaseOutcome>,
    pub deterministic: Confusion,
    pub learned: Confusion,
    pub guarded: Confusion,
    pub strict: Confusion,
}

/// Runs every policy over every case. Raw learned answers are recorded separately from the
/// guarded ones.
pub fn evaluate(model: Option<&LocalDecisionModel>, cases: &[Labeled]) -> Evaluation {
    let decider = LocalDecider::new(model.cloned(), PolicyMode::LearnedGuarded);
    let mut evaluation = Evaluation {
        cases: Vec::with_capacity(cases.len()),
        deterministic: Confusion::default(),
        learned: Confusion::default(),
        guarded: Confusion::default(),
        strict: Confusion::default(),
    };
    for case in cases {
        let outcome = decider.decide(&case.state);
        let raw = outcome.learned_raw.unwrap_or(Decision::Escalate);
        evaluation
            .deterministic
            .record(outcome.deterministic, case.expected);
        evaluation.learned.record(raw, case.expected);
        evaluation.guarded.record(outcome.guarded, case.expected);
        evaluation.strict.record(outcome.strict, case.expected);
        evaluation.cases.push(CaseOutcome {
            id: case.id.clone(),
            expected: case.expected,
            deterministic: outcome.deterministic,
            learned_raw: outcome.learned_raw,
            guarded: outcome.guarded,
            strict: outcome.strict,
        });
    }
    evaluation
}

impl Evaluation {
    /// Combines evaluations (for example leave-one-out folds) into one.
    pub fn merge(parts: impl IntoIterator<Item = Evaluation>) -> Evaluation {
        let mut all = Evaluation {
            cases: Vec::new(),
            deterministic: Confusion::default(),
            learned: Confusion::default(),
            guarded: Confusion::default(),
            strict: Confusion::default(),
        };
        for part in parts {
            all.cases.extend(part.cases);
            for (into, from) in [
                (&mut all.deterministic, part.deterministic),
                (&mut all.learned, part.learned),
                (&mut all.guarded, part.guarded),
                (&mut all.strict, part.strict),
            ] {
                into.tp += from.tp;
                into.fp += from.fp;
                into.fn_ += from.fn_;
                into.tn += from.tn;
            }
        }
        all
    }

    /// Cases the baseline escalates, that should continue, and that the guarded policy
    /// continues: the additional safe local decisions.
    pub fn newly_resolved(&self) -> Vec<&CaseOutcome> {
        self.cases
            .iter()
            .filter(|c| {
                c.deterministic == Decision::Escalate
                    && c.guarded == Decision::Continue
                    && c.expected == Decision::Continue
            })
            .collect()
    }

    /// Cases the learned model would continue that should have escalated, whether or not the
    /// guard caught them. Never hidden.
    pub fn unsafe_learned(&self) -> Vec<&CaseOutcome> {
        self.cases
            .iter()
            .filter(|c| {
                c.learned_raw == Some(Decision::Continue) && c.expected == Decision::Escalate
            })
            .collect()
    }

    /// Cases the learned model would continue where the baseline escalates (right or wrong).
    pub fn learned_beyond_baseline(&self) -> Vec<&CaseOutcome> {
        self.cases
            .iter()
            .filter(|c| {
                c.deterministic == Decision::Escalate && c.learned_raw == Some(Decision::Continue)
            })
            .collect()
    }

    /// `guarded local - deterministic local`: extra decisions resolved without escalating.
    pub fn local_gain(&self) -> isize {
        self.guarded.local() as isize - self.deterministic.local() as isize
    }

    /// `guarded true continues - deterministic true continues`: extra *correct* local
    /// decisions. Equal to `local_gain` exactly when the guarded policy adds no false continue.
    pub fn safe_local_gain(&self) -> isize {
        self.guarded.tp as isize - self.deterministic.tp as isize
    }
}

fn pct(x: f64) -> String {
    format!("{:.1}%", x * 100.0)
}

/// One policy's confusion matrix and headline metrics.
pub fn render_confusion(name: &str, c: &Confusion) -> String {
    format!(
        "{name}\n\
         {:<22}{:>18}{:>18}\n\
         {:<22}{:>18}{:>18}\n\
         {:<22}{:>18}{:>18}\n\
         \x20 local decision coverage {}   escalation rate {}   accuracy {}\n\
         \x20 false continues {} ({} of all, {} of should-escalate)   safe local (true continues) {}\n",
        "",
        "Expected Continue",
        "Expected Escalate",
        "Pred Continue",
        format!("TP {}", c.tp),
        format!("FP {}  <- false continue", c.fp),
        "Pred Escalate",
        format!("FN {}", c.fn_),
        format!("TN {}", c.tn),
        pct(c.local_decision_coverage()),
        pct(c.escalation_rate()),
        pct(c.accuracy()),
        c.fp,
        pct(c.false_continue_rate()),
        pct(c.false_continue_given_escalate()),
        c.tp,
    )
}

impl Evaluation {
    pub fn render(&self, title: &str) -> String {
        let mut out = format!(
            "{title}: {} decisions, ceiling on safe local coverage {}\n\n",
            self.cases.len(),
            pct(self.deterministic.coverage_ceiling())
        );
        out.push_str(&render_confusion("Deterministic", &self.deterministic));
        out.push('\n');
        out.push_str(&render_confusion("Learned (raw, unguarded)", &self.learned));
        out.push('\n');
        out.push_str(&render_confusion("LearnedGuarded", &self.guarded));
        out.push('\n');
        out.push_str(&render_confusion(
            "LearnedStrict (learned AND deterministic)",
            &self.strict,
        ));
        out.push_str(&format!(
            "\nlocal_gain (guarded local - deterministic local): {}\nadditional safe local decisions (guarded true continues - deterministic): {}\n",
            self.local_gain(),
            self.safe_local_gain()
        ));
        let ids = |v: Vec<&CaseOutcome>| {
            if v.is_empty() {
                "none".to_string()
            } else {
                v.iter()
                    .map(|c| c.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        };
        out.push_str(&format!("newly resolved: {}\n", ids(self.newly_resolved())));
        out.push_str(&format!(
            "learned beyond baseline: {}\n",
            ids(self.learned_beyond_baseline())
        ));
        out.push_str(&format!(
            "unsafe learned continues (caught or not): {}\n",
            ids(self.unsafe_learned())
        ));
        out
    }
}
