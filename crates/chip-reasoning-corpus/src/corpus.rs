//! The corpus itself. Plain Rust data, reviewable in Git.
//!
//! Vocabulary of facts used in `ReasoningInput::inputs` (all explicit; a case never
//! depends on anything outside its input):
//! `change_affects_capability`, `prerequisites_met`, `last_outcome` ("success" | "failure"),
//! `read_only`, `idempotent`, `requires_approval`, `approval_granted`, `sources_agree`,
//! `reported_failure`, `state_changed`, `escalate_requested`, `policy_ambiguous`,
//! `requires_broader_knowledge`, `competing_interpretations`.
//!
//! Evidence is handled by Chip before any reasoner runs: `KnownValid` evidence is reused
//! and the reasoner is not consulted. The corpus still records the expected answer so
//! a reasoner that is consulted anyway can be checked.

use std::collections::BTreeMap;

use chip_core::{CapabilityId, EvidenceState, InputValue, LocalReasoningResult, ReasoningInput};

/// The normalized verdict. Verdict text is never part of an evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Verdict {
    Continue,
    Escalate,
}

impl Verdict {
    pub fn of(result: &LocalReasoningResult) -> Verdict {
        match result {
            LocalReasoningResult::Continue { .. } => Verdict::Continue,
            LocalReasoningResult::Escalate { .. } => Verdict::Escalate,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Verdict::Continue => "CONTINUE",
            Verdict::Escalate => "ESCALATE",
        }
    }
}

/// Why a case exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Category {
    Deterministic,
    LocalJudgment,
    InsufficientInformation,
    ConflictingInformation,
    ExplicitEscalation,
}

/// What kind of reasoner should be responsible for the case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Classification {
    /// Answerable directly from Chip's known state; no model needed.
    Deterministic,
    /// Bounded, all facts present; a small local reasoner may reasonably decide.
    LocalJudgment,
    /// Must not be decided locally; the right answer is to escalate.
    EscalationRequired,
}

impl Category {
    pub fn name(self) -> &'static str {
        match self {
            Category::Deterministic => "deterministic",
            Category::LocalJudgment => "local_judgment",
            Category::InsufficientInformation => "insufficient_information",
            Category::ConflictingInformation => "conflicting_information",
            Category::ExplicitEscalation => "explicit_escalation",
        }
    }

    pub fn classification(self) -> Classification {
        match self {
            Category::Deterministic => Classification::Deterministic,
            Category::LocalJudgment => Classification::LocalJudgment,
            _ => Classification::EscalationRequired,
        }
    }

    pub const ALL: [Category; 5] = [
        Category::Deterministic,
        Category::LocalJudgment,
        Category::InsufficientInformation,
        Category::ConflictingInformation,
        Category::ExplicitEscalation,
    ];
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReasoningCase {
    pub id: &'static str,
    pub category: Category,
    pub input: ReasoningInput,
    pub expected: Verdict,
    /// For humans reading evaluation output; never part of any decision.
    pub reason: &'static str,
}

fn t(text: &str) -> InputValue {
    InputValue::Text(text.to_string())
}

fn b(flag: bool) -> InputValue {
    InputValue::Bool(flag)
}

fn n(number: i64) -> InputValue {
    InputValue::Integer(number)
}

fn case(
    id: &'static str,
    category: Category,
    evidence: EvidenceState,
    capability: &str,
    facts: &[(&str, InputValue)],
    expected: Verdict,
    reason: &'static str,
) -> ReasoningCase {
    ReasoningCase {
        id,
        category,
        input: ReasoningInput {
            capability: CapabilityId::new(capability).expect("corpus capability ids are valid"),
            inputs: facts
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect::<BTreeMap<_, _>>(),
            evidence,
        },
        expected,
        reason,
    }
}

/// The corpus: 32 cases. Built fresh on every call; identical every time.
pub fn corpus() -> Vec<ReasoningCase> {
    use Category::*;
    use EvidenceState::*;
    use Verdict::*;
    vec![
        // Deterministic: Chip's own state decides.
        case(
            "det-valid-01",
            Deterministic,
            KnownValid,
            "compute.selftest",
            &[],
            Continue,
            "valid evidence is reused; the reasoner is not consulted",
        ),
        case(
            "det-valid-02",
            Deterministic,
            KnownValid,
            "tests.run",
            &[("source_revision", t("abc123"))],
            Continue,
            "valid evidence for these exact inputs",
        ),
        case(
            "det-valid-03",
            Deterministic,
            KnownValid,
            "deploy.status",
            &[("prerequisites_met", b(false))],
            Continue,
            "valid evidence wins over every other fact",
        ),
        case(
            "det-stale-01",
            Deterministic,
            KnownStale,
            "compute.selftest",
            &[],
            Escalate,
            "stale evidence and no facts that justify continuing",
        ),
        case(
            "det-stale-02",
            Deterministic,
            KnownStale,
            "tests.run",
            &[("source_revision", t("abc123"))],
            Escalate,
            "stale evidence; the revision alone says nothing about relevance",
        ),
        case(
            "det-unknown-01",
            Deterministic,
            Unknown,
            "compute.selftest",
            &[],
            Escalate,
            "no evidence and no facts",
        ),
        case(
            "det-unknown-02",
            Deterministic,
            Unknown,
            "build.run",
            &[("target", t("release"))],
            Escalate,
            "no evidence; a target alone justifies nothing",
        ),
        case(
            "det-unknown-03",
            Deterministic,
            Unknown,
            "deploy.status",
            &[],
            Escalate,
            "no evidence for a different capability",
        ),
        // Local judgment: all relevant facts are present and bounded.
        case(
            "lj-01",
            LocalJudgment,
            KnownStale,
            "tests.run",
            &[
                ("change_affects_capability", b(false)),
                ("prerequisites_met", b(true)),
            ],
            Continue,
            "state changed but not in a way this capability depends on, and prerequisites hold",
        ),
        case(
            "lj-02",
            LocalJudgment,
            KnownStale,
            "tests.run",
            &[
                ("change_affects_capability", b(true)),
                ("prerequisites_met", b(true)),
            ],
            Escalate,
            "the change affects this capability",
        ),
        case(
            "lj-03",
            LocalJudgment,
            KnownStale,
            "tests.run",
            &[
                ("change_affects_capability", b(false)),
                ("prerequisites_met", b(false)),
            ],
            Escalate,
            "irrelevant change, but prerequisites are not met",
        ),
        case(
            "lj-04",
            LocalJudgment,
            KnownStale,
            "tests.run",
            &[
                ("change_affects_capability", b(false)),
                ("prerequisites_met", b(true)),
                ("last_outcome", t("success")),
            ],
            Continue,
            "irrelevant change after a successful outcome",
        ),
        case(
            "lj-05",
            LocalJudgment,
            KnownStale,
            "tests.run",
            &[
                ("change_affects_capability", b(false)),
                ("prerequisites_met", b(true)),
                ("last_outcome", t("failure")),
            ],
            Escalate,
            "a previous failure should not be silently carried forward",
        ),
        case(
            "lj-06",
            LocalJudgment,
            Unknown,
            "build.run",
            &[
                ("read_only", b(true)),
                ("idempotent", b(true)),
                ("prerequisites_met", b(true)),
            ],
            Continue,
            "no evidence, but the operation is read-only, idempotent and ready",
        ),
        case(
            "lj-07",
            LocalJudgment,
            Unknown,
            "build.run",
            &[
                ("read_only", b(false)),
                ("idempotent", b(true)),
                ("prerequisites_met", b(true)),
            ],
            Escalate,
            "not read-only",
        ),
        case(
            "lj-08",
            LocalJudgment,
            Unknown,
            "build.run",
            &[
                ("read_only", b(true)),
                ("idempotent", b(false)),
                ("prerequisites_met", b(true)),
            ],
            Escalate,
            "not idempotent",
        ),
        case(
            "lj-09",
            LocalJudgment,
            KnownStale,
            "deploy.status",
            &[
                ("change_affects_capability", b(false)),
                ("prerequisites_met", b(true)),
                ("requires_approval", b(true)),
                ("approval_granted", b(true)),
            ],
            Continue,
            "approval is required and has been granted",
        ),
        case(
            "lj-10",
            LocalJudgment,
            KnownStale,
            "deploy.status",
            &[
                ("change_affects_capability", b(false)),
                ("prerequisites_met", b(true)),
                ("requires_approval", b(true)),
                ("approval_granted", b(false)),
            ],
            Escalate,
            "approval is required and has not been granted",
        ),
        // Insufficient information: the needed facts are not all present.
        case(
            "ii-01",
            InsufficientInformation,
            KnownStale,
            "tests.run",
            &[("change_affects_capability", b(false))],
            Escalate,
            "prerequisites are not stated",
        ),
        case(
            "ii-02",
            InsufficientInformation,
            KnownStale,
            "tests.run",
            &[("prerequisites_met", b(true))],
            Escalate,
            "whether the change matters is not stated",
        ),
        case(
            "ii-03",
            InsufficientInformation,
            Unknown,
            "build.run",
            &[("read_only", b(true))],
            Escalate,
            "idempotence and readiness are not stated",
        ),
        case(
            "ii-04",
            InsufficientInformation,
            Unknown,
            "deploy.apply",
            &[],
            Escalate,
            "nothing is known about this operation",
        ),
        case(
            "ii-05",
            InsufficientInformation,
            KnownStale,
            "tests.run",
            &[("last_outcome", t("success"))],
            Escalate,
            "a past success alone does not show the state is unchanged",
        ),
        // Conflicting information: facts contradict each other.
        case(
            "cf-01",
            ConflictingInformation,
            KnownStale,
            "tests.run",
            &[
                ("change_affects_capability", b(false)),
                ("prerequisites_met", b(true)),
                ("sources_agree", b(false)),
            ],
            Escalate,
            "otherwise continuable, but the sources disagree",
        ),
        case(
            "cf-02",
            ConflictingInformation,
            KnownStale,
            "tests.run",
            &[
                ("change_affects_capability", b(false)),
                ("prerequisites_met", b(true)),
                ("last_outcome", t("success")),
                ("reported_failure", b(true)),
            ],
            Escalate,
            "recorded success conflicts with a reported failure",
        ),
        case(
            "cf-03",
            ConflictingInformation,
            Unknown,
            "build.run",
            &[
                ("read_only", b(true)),
                ("idempotent", b(true)),
                ("prerequisites_met", b(true)),
                ("sources_agree", b(false)),
            ],
            Escalate,
            "readiness facts come from sources that disagree",
        ),
        case(
            "cf-04",
            ConflictingInformation,
            KnownStale,
            "tests.run",
            &[
                ("change_affects_capability", b(false)),
                ("state_changed", b(true)),
                ("prerequisites_met", b(true)),
            ],
            Escalate,
            "state is reported as changed yet the change is said not to matter",
        ),
        case(
            "cf-05",
            ConflictingInformation,
            KnownStale,
            "deploy.status",
            &[
                ("change_affects_capability", b(false)),
                ("prerequisites_met", b(true)),
                ("reported_failure", b(true)),
            ],
            Escalate,
            "prerequisites hold but a failure is reported",
        ),
        // Explicit escalation: the case itself calls for a capable reasoner.
        case(
            "ex-01",
            ExplicitEscalation,
            KnownStale,
            "tests.run",
            &[
                ("change_affects_capability", b(false)),
                ("prerequisites_met", b(true)),
                ("escalate_requested", b(true)),
            ],
            Escalate,
            "escalation was explicitly requested",
        ),
        case(
            "ex-02",
            ExplicitEscalation,
            Unknown,
            "build.run",
            &[
                ("read_only", b(true)),
                ("idempotent", b(true)),
                ("prerequisites_met", b(true)),
                ("policy_ambiguous", b(true)),
            ],
            Escalate,
            "the applicable policy is ambiguous",
        ),
        case(
            "ex-03",
            ExplicitEscalation,
            KnownStale,
            "tests.run",
            &[
                ("change_affects_capability", b(false)),
                ("prerequisites_met", b(true)),
                ("requires_broader_knowledge", b(true)),
            ],
            Escalate,
            "the decision needs knowledge beyond the stated facts",
        ),
        case(
            "ex-04",
            ExplicitEscalation,
            KnownStale,
            "tests.run",
            &[
                ("change_affects_capability", b(false)),
                ("prerequisites_met", b(true)),
                ("competing_interpretations", n(2)),
            ],
            Escalate,
            "more than one competing interpretation",
        ),
    ]
}
