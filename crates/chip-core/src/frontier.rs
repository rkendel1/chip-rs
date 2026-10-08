//! The Decision Frontier: what a piece of work has left unresolved.
//!
//! The runtime owns it entirely. It is work-local state, kept for one work and then reported, and the
//! model neither writes it nor is asked to maintain it: Chip derives it from the work's own
//! requirements and updates it from authoritative observations.
//!
//! ```text
//! KNOWN        what observations, executions and goal evaluation already establish (not duplicated here)
//! FRONTIER     unresolved questions that bear on the next safe decision (this module)
//! UNKNOWN      everything else (not represented)
//! ```
//!
//! A frontier item is a question, a kind, a status and, once it has left `Open`, a reference to the
//! execution whose observation moved it. It is **not** a fact database and carries no score, priority,
//! owner, dependency or timestamp. What resolves an item is an [`ObservationPredicate`], the machinery
//! the runtime already uses to decide whether an observation establishes an outcome, so a transition is
//! always grounded in something that actually happened and never in a model's words.
//!
//! # Transitions
//!
//! * **Resolved**: the item's predicate came to hold because of this execution's observation.
//! * **Invalidated**: the item had been resolved and a later observation made its predicate stop
//!   holding (a verification superseded by a later change, for example). The runtime then opens a
//!   successor item with the same question: an invalidated assumption never stays silently open.
//! * **Opened**: at the start of the work, for each requirement; for a successor; and for a failed
//!   execution, which opens the question whether that capability succeeds after having failed.
//!
//! Resolving an item is **not** goal satisfaction, **not** verification and **not** useful work. They are
//! decided elsewhere and none of them reads the frontier.

use std::fmt;
use std::sync::Arc;

use crate::{
    CapabilityId, CapabilityRequest, ExecutionId, ExecutionStatus, Observation, ObservationKind,
    ObservationPredicate,
};

/// A frontier item's identity: assigned by the runtime, in the order items are opened, within one work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FrontierItemId(u32);

impl FrontierItemId {
    /// The item's number within its work, from 1.
    pub fn number(self) -> u32 {
        self.0
    }
}

impl fmt::Display for FrontierItemId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "F{}", self.0)
    }
}

/// What kind of unresolved question an item is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrontierKind {
    /// Something that has not been observed.
    MissingEvidence,
    /// Evidence supports more than one reading.
    Ambiguity,
    /// Something is believed and not yet verified.
    UnverifiedHypothesis,
    /// The work needs a capability it was not given.
    MissingCapability,
    /// The work needs an authority it was not given.
    MissingAuthority,
}

impl FrontierKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::MissingEvidence => "missing_evidence",
            Self::Ambiguity => "ambiguity",
            Self::UnverifiedHypothesis => "unverified_hypothesis",
            Self::MissingCapability => "missing_capability",
            Self::MissingAuthority => "missing_authority",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrontierStatus {
    Open,
    Resolved,
    /// Had been resolved; later evidence showed it no longer holds.
    Invalidated,
}

impl FrontierStatus {
    pub fn name(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Resolved => "resolved",
            Self::Invalidated => "invalidated",
        }
    }
}

/// The existing runtime evidence a transition rests on: the execution whose observation moved the
/// item. The frontier refers to it; it does not copy it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontierResolution {
    pub evidence: ExecutionId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontierItem {
    pub id: FrontierItemId,
    pub question: String,
    pub kind: FrontierKind,
    pub status: FrontierStatus,
    /// Set when the item leaves `Open`: the execution that resolved or invalidated it.
    pub resolution: Option<FrontierResolution>,
}

/// The unresolved questions of one work, in the order they were opened.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DecisionFrontier {
    items: Vec<FrontierItem>,
}

impl DecisionFrontier {
    pub fn items(&self) -> &[FrontierItem] {
        &self.items
    }

    pub fn get(&self, id: FrontierItemId) -> Option<&FrontierItem> {
        self.items.iter().find(|i| i.id == id)
    }

    fn count(&self, status: FrontierStatus) -> usize {
        self.items.iter().filter(|i| i.status == status).count()
    }

    /// Items ever opened, including successors and items opened by failures.
    pub fn opened(&self) -> usize {
        self.items.len()
    }

    pub fn resolved(&self) -> usize {
        self.count(FrontierStatus::Resolved)
    }

    pub fn invalidated(&self) -> usize {
        self.count(FrontierStatus::Invalidated)
    }

    /// Items still open: the questions this work has not yet answered.
    pub fn remaining(&self) -> usize {
        self.count(FrontierStatus::Open)
    }
}

/// A frontier item a caller declares for a kind of work: the question, and the predicate over the
/// recorded observations that decides whether it has been answered.
#[derive(Debug, Clone)]
pub struct FrontierItemSpec {
    pub kind: FrontierKind,
    pub question: String,
    pub resolved_by: Arc<dyn ObservationPredicate>,
}

impl FrontierItemSpec {
    pub fn new(
        kind: FrontierKind,
        question: impl Into<String>,
        resolved_by: Arc<dyn ObservationPredicate>,
    ) -> Self {
        Self {
            kind,
            question: question.into(),
            resolved_by,
        }
    }
}

/// "A required output was produced": an observation of a successful execution whose output is exactly
/// the output. What the runtime already means by a required output, as a frontier question.
#[derive(Debug, Clone)]
pub(crate) struct OutputProduced(pub String);

impl ObservationPredicate for OutputProduced {
    fn describe(&self) -> String {
        format!("the required output {:?} was produced", self.0)
    }

    fn satisfied_by(&self, observation: &Observation) -> bool {
        observation.kind == ObservationKind::ExecutionCompleted
            && observation.status == ExecutionStatus::Success
            && observation.output.as_deref().map(str::trim) == Some(self.0.trim())
    }
}

/// A transition the tracker made, for the work loop to record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Transition {
    Opened {
        item: FrontierItemId,
        kind: FrontierKind,
        question: String,
    },
    Resolved {
        item: FrontierItemId,
        evidence: ExecutionId,
    },
    Invalidated {
        item: FrontierItemId,
        evidence: ExecutionId,
    },
}

enum Resolver {
    /// Answered when the predicate holds over the recorded observations.
    Predicate(Arc<dyn ObservationPredicate>),
    /// Opened by a failed execution; answered by a later success of the same capability.
    Recovers(CapabilityId),
}

struct Tracked {
    item: FrontierItem,
    resolver: Resolver,
}

/// The runtime's working copy of the frontier for one work.
pub(crate) struct FrontierTracker {
    tracked: Vec<Tracked>,
    next: u32,
}

impl FrontierTracker {
    /// Opens one item per declared requirement. Their opening is the first thing recorded.
    pub(crate) fn new(specs: &[FrontierItemSpec]) -> (Self, Vec<Transition>) {
        let mut tracker = Self {
            tracked: Vec::new(),
            next: 1,
        };
        let opened = specs
            .iter()
            .map(|s| {
                tracker.open(
                    s.kind,
                    s.question.clone(),
                    Resolver::Predicate(s.resolved_by.clone()),
                )
            })
            .collect();
        (tracker, opened)
    }

    fn open(&mut self, kind: FrontierKind, question: String, resolver: Resolver) -> Transition {
        let id = FrontierItemId(self.next);
        self.next += 1;
        self.tracked.push(Tracked {
            item: FrontierItem {
                id,
                question: question.clone(),
                kind,
                status: FrontierStatus::Open,
                resolution: None,
            },
            resolver,
        });
        Transition::Opened {
            item: id,
            kind,
            question,
        }
    }

    /// Applies one execution's observation. `observations` includes it, last. Returns what changed, in
    /// the order resolved, invalidated, opened.
    pub(crate) fn observe(
        &mut self,
        request: &CapabilityRequest,
        observation: &Observation,
        observations: &[Observation],
    ) -> Vec<Transition> {
        let succeeded = observation.kind == ObservationKind::ExecutionCompleted
            && observation.status == ExecutionStatus::Success;
        let evidence = observation.execution_id.clone();
        let (mut resolved, mut invalidated, mut successors) = (Vec::new(), Vec::new(), Vec::new());
        for t in &mut self.tracked {
            match (&t.item.status, &t.resolver) {
                (FrontierStatus::Open, Resolver::Predicate(p))
                    if p.satisfied_by_trajectory(observations) =>
                {
                    t.item.status = FrontierStatus::Resolved;
                    t.item.resolution = Some(FrontierResolution {
                        evidence: evidence.clone(),
                    });
                    resolved.push(Transition::Resolved {
                        item: t.item.id,
                        evidence: evidence.clone(),
                    });
                }
                (FrontierStatus::Open, Resolver::Recovers(capability))
                    if succeeded && *capability == request.capability_id =>
                {
                    t.item.status = FrontierStatus::Resolved;
                    t.item.resolution = Some(FrontierResolution {
                        evidence: evidence.clone(),
                    });
                    resolved.push(Transition::Resolved {
                        item: t.item.id,
                        evidence: evidence.clone(),
                    });
                }
                // Resolved by something a later observation has superseded: the assumption is gone.
                (FrontierStatus::Resolved, Resolver::Predicate(p))
                    if !p.satisfied_by_trajectory(observations) =>
                {
                    t.item.status = FrontierStatus::Invalidated;
                    t.item.resolution = Some(FrontierResolution {
                        evidence: evidence.clone(),
                    });
                    invalidated.push(Transition::Invalidated {
                        item: t.item.id,
                        evidence: evidence.clone(),
                    });
                    successors.push((t.item.kind, t.item.question.clone(), p.clone()));
                }
                _ => {}
            }
        }
        let mut changes = resolved;
        changes.extend(invalidated);
        for (kind, question, predicate) in successors {
            changes.push(self.open(kind, question, Resolver::Predicate(predicate)));
        }
        // A failed execution changes the frontier: whether that capability can succeed is now unresolved.
        // One open question per capability, however many times it fails before it succeeds.
        let failed = observation.kind == ObservationKind::ExecutionFailed
            || observation.status != ExecutionStatus::Success;
        if failed {
            let capability = &request.capability_id;
            let already = self.tracked.iter().any(|t| {
                t.item.status == FrontierStatus::Open
                    && matches!(&t.resolver, Resolver::Recovers(c) if c == capability)
            });
            if !already {
                changes.push(self.open(
                    FrontierKind::MissingEvidence,
                    format!(
                        "Does {capability} succeed after execution {} failed?",
                        evidence
                    ),
                    Resolver::Recovers(capability.clone()),
                ));
            }
        }
        changes
    }

    pub(crate) fn into_frontier(self) -> DecisionFrontier {
        DecisionFrontier {
            items: self.tracked.into_iter().map(|t| t.item).collect(),
        }
    }
}
