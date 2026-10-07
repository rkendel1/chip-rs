//! A structured corpus of local decisions, produced by a versioned deterministic generator.
//!
//! The generator ([`generate`]) is the source of truth. The same [`GeneratorConfig`] and
//! [`GENERATOR_VERSION`] always yield byte-identical corpus text ([`Corpus::to_jsonl`]) and
//! digest ([`Corpus::digest`]); any change to what the generator emits must bump the version.
//! The committed corpus file is a *checked artifact*: a test regenerates it and compares bytes.
//!
//! # Decision patterns
//!
//! A case is one concrete context for one *decision pattern*: a fixed combination of evidence,
//! impact and decision-relevant facts with an explicit expected decision and a human-readable
//! rationale. Contexts vary independently of the label: the capability, the opaque graph token
//! and a set of irrelevant facts drawn from a small finite pool. Rationale, pattern id, family,
//! tags and split are evaluation metadata. None of it reaches a model's features.
//!
//! # The labelling rules
//!
//! These are written as data (the patterns), not hidden in a function, so each is reviewable:
//!
//! * **Valid evidence reuses** (the baseline's authority): valid evidence and an unchanged
//!   capability continue whatever the other facts say, and valid evidence for an impacted
//!   capability escalates. Facts never override valid evidence, so the deterministic baseline is
//!   never wrong in this corpus. (Escalating on valid evidence would make the baseline itself
//!   produce false continues.)
//! * **Judgment** lives under stale or unknown evidence, and only for an *unchanged* capability:
//!   - stale evidence continues when prerequisites are met, the last outcome is not a failure or
//!     unknown, and any approval the operation requires has been granted;
//!   - unknown evidence continues only for an operation that is read-only, idempotent and ready.
//! * **Impacted** capabilities always escalate. **Explicitly unknown, unavailable or ambiguous**
//!   critical facts escalate, and are never treated as `false` or as absence. A **missing**
//!   required fact escalates. **Conflicts** (an approval both granted and revoked, sources that
//!   disagree, a reported failure, a state reported changed, an explicit escalation request, an
//!   ambiguous policy) escalate.

use std::collections::{BTreeMap, BTreeSet};

use chip_core::{
    CapabilityDecisionState, CapabilityId, EvidenceState, GraphStateToken, ImpactState, InputValue,
};
use sha2::{Digest, Sha256};

pub const CORPUS_SCHEMA: &str = "chip.decision-corpus.v1";
pub const GENERATOR_VERSION: &str = "chip.decision-corpus-gen.v1";

/// Capabilities used for contexts. The last two are reserved for the capability holdout.
pub const CAPABILITIES: [&str; 12] = [
    "tests.run",
    "build.run",
    "deploy.status",
    "deploy.apply",
    "lint.run",
    "docs.build",
    "package.sign",
    "cache.warm",
    "db.migrate",
    "index.refresh",
    "report.render",
    "queue.drain",
];
pub const HELD_OUT_CAPABILITIES: [&str; 2] = ["report.render", "queue.drain"];

/// Spellings of "the fact is not known". Never `false`, never absence.
pub const UNKNOWN_SPELLINGS: [&str; 3] = ["unknown", "unavailable", "ambiguous"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GeneratorConfig {
    pub seed: u64,
    /// Contexts generated for each pattern the learned model is meant to recover.
    pub variants_per_target_positive: usize,
    /// Contexts for patterns the baseline already settles as `Continue`.
    pub variants_per_baseline_positive: usize,
    /// Contexts for every negative pattern.
    pub variants_per_negative: usize,
}

impl GeneratorConfig {
    pub const V1: GeneratorConfig = GeneratorConfig {
        seed: 42,
        variants_per_target_positive: 16,
        variants_per_baseline_positive: 8,
        variants_per_negative: 6,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Family {
    ValidReuse,
    Readiness,
    Outcome,
    Approval,
    Operation,
    Conflict,
}

impl Family {
    pub fn name(self) -> &'static str {
        match self {
            Family::ValidReuse => "valid-reuse",
            Family::Readiness => "readiness",
            Family::Outcome => "outcome",
            Family::Approval => "approval",
            Family::Operation => "operation",
            Family::Conflict => "conflict",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum FactValue {
    Bool(bool),
    Int(i64),
    Text(String),
}

impl FactValue {
    fn to_input(&self) -> InputValue {
        match self {
            FactValue::Bool(b) => InputValue::Bool(*b),
            FactValue::Int(i) => InputValue::Integer(*i),
            FactValue::Text(t) => InputValue::Text(t.clone()),
        }
    }

    fn is_unknown(&self) -> bool {
        matches!(self, FactValue::Text(t) if UNKNOWN_SPELLINGS.contains(&t.as_str()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Split {
    Train,
    Validation,
    HeldOut,
}

impl Split {
    pub fn name(self) -> &'static str {
        match self {
            Split::Train => "train",
            Split::Validation => "validation",
            Split::HeldOut => "held_out",
        }
    }
}

/// Which generalization dimensions withhold a case from training.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Holdouts {
    /// The case itself was drawn out at random; its pattern and context are seen in training.
    pub case: bool,
    /// The case's whole pattern is withheld: a fact combination the model never saw together.
    pub pattern: bool,
    /// The case's context (a capability with a combination of irrelevant facts) is withheld.
    pub context: bool,
    /// The case's capability appears nowhere in training.
    pub capability: bool,
}

impl Holdouts {
    pub fn any(&self) -> bool {
        self.case || self.pattern || self.context || self.capability
    }
}

/// Evaluation families, assigned structurally from the patterns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Tags {
    /// A negative that differs from some positive pattern in exactly one decision factor.
    pub hard_negative: bool,
    /// The label depends on several facts jointly.
    pub conjunction: bool,
    /// Contradictory information is present.
    pub conflict: bool,
    /// A critical fact is explicitly unknown, unavailable or ambiguous.
    pub unknown: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorpusCase {
    pub case_id: String,
    pub family: Family,
    pub pattern_id: String,
    pub expected_continue: bool,
    pub evidence: EvidenceState,
    pub impact: ImpactState,
    pub capability: String,
    /// Opaque: derived from the context only, never from the pattern or the label.
    pub graph_state: [u8; 32],
    pub facts: BTreeMap<String, FactValue>,
    /// For reviewers. Never a model input.
    pub rationale: String,
    pub context_id: String,
    pub split: Split,
    pub holdouts: Holdouts,
    pub tags: Tags,
}

impl CorpusCase {
    /// The decision state a model sees. Rationale, pattern, family, tags and split are absent.
    pub fn decision_state(&self) -> CapabilityDecisionState {
        let mut state = CapabilityDecisionState::new(
            CapabilityId::new(self.capability.clone()).expect("corpus capability ids are valid"),
            GraphStateToken::from_digest(self.graph_state),
            self.evidence,
            self.impact,
        );
        for (name, value) in &self.facts {
            state = state.with_input(name.clone(), value.to_input());
        }
        state
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Corpus {
    pub generator: &'static str,
    pub config: GeneratorConfig,
    pub cases: Vec<CorpusCase>,
}

// ---------------------------------------------------------------------------------------------
// Patterns
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ev {
    Valid,
    Stale,
    Unknown,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Im {
    Unchanged,
    Impacted,
}

/// One slot of a decision-relevant fact in a pattern.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Slot {
    True,
    False,
    /// Explicitly unknown; the spelling varies per case.
    Unknown,
    Absent,
    Text(&'static str),
}

struct Pattern {
    id: String,
    family: Family,
    evidence: Ev,
    impact: Im,
    /// Decision-relevant facts. `Absent` slots are simply omitted.
    facts: Vec<(&'static str, Slot)>,
    expected_continue: bool,
    rationale: String,
    conjunction: bool,
    conflict: bool,
}

fn ev_name(e: Ev) -> &'static str {
    match e {
        Ev::Valid => "valid",
        Ev::Stale => "stale",
        Ev::Unknown => "unknown",
    }
}

fn im_name(i: Im) -> &'static str {
    match i {
        Im::Unchanged => "unchanged",
        Im::Impacted => "impacted",
    }
}

fn slot_name(s: Slot) -> &'static str {
    match s {
        Slot::True => "true",
        Slot::False => "false",
        Slot::Unknown => "unknown",
        Slot::Absent => "absent",
        Slot::Text(t) => t,
    }
}

/// How a slot reads in a rationale.
fn slot_phrase(name: &str, s: Slot) -> String {
    match s {
        Slot::True => format!("{name} is true"),
        Slot::False => format!("{name} is false"),
        Slot::Unknown => format!("{name} is explicitly unknown"),
        Slot::Absent => format!("{name} is not stated"),
        Slot::Text(t) => format!("{name} is {t}"),
    }
}

fn rationale(ev: Ev, im: Im, facts: &[(&'static str, Slot)], expected: bool, why: &str) -> String {
    let mut parts = vec![
        format!("evidence is {}", ev_name(ev)),
        format!("the capability is {}", im_name(im)),
    ];
    parts.extend(facts.iter().map(|(n, s)| slot_phrase(n, *s)));
    format!(
        "{}: {}; {}.",
        if expected { "Continue" } else { "Escalate" },
        parts.join(", "),
        why
    )
}

fn patterns() -> Vec<Pattern> {
    use Slot::*;
    let mut out: Vec<Pattern> = Vec::new();
    let mut push = |family: Family,
                    id: String,
                    evidence: Ev,
                    impact: Im,
                    facts: Vec<(&'static str, Slot)>,
                    expected_continue: bool,
                    why: &str,
                    conjunction: bool,
                    conflict: bool| {
        let rationale = rationale(evidence, impact, &facts, expected_continue, why);
        out.push(Pattern {
            id,
            family,
            evidence,
            impact,
            facts,
            expected_continue,
            rationale,
            conjunction,
            conflict,
        });
    };

    // Valid evidence: reused for an unchanged capability whatever else is stated; never for an
    // impacted one.
    let valid_reuse: [(&str, Vec<(&'static str, Slot)>); 4] = [
        ("bare", vec![]),
        ("prereq-false", vec![("prerequisites_met", False)]),
        ("prereq-unknown", vec![("prerequisites_met", Unknown)]),
        ("read-only-false", vec![("read_only", False)]),
    ];
    for (label, facts) in valid_reuse {
        push(
            Family::ValidReuse,
            format!("valid-reuse/{label}.unchanged"),
            Ev::Valid,
            Im::Unchanged,
            facts,
            true,
            "valid evidence for these exact inputs is reused and wins over the other facts",
            false,
            false,
        );
    }
    push(
        Family::ValidReuse,
        "valid-reuse/bare.impacted".into(),
        Ev::Valid,
        Im::Impacted,
        vec![],
        false,
        "the capability's relevant state changed, so valid evidence cannot be reused",
        false,
        false,
    );
    push(
        Family::ValidReuse,
        "valid-reuse/prereq-true.impacted".into(),
        Ev::Valid,
        Im::Impacted,
        vec![("prerequisites_met", True)],
        false,
        "ready prerequisites do not make evidence for a changed capability reusable",
        false,
        false,
    );

    // Readiness: stale evidence continues only when the capability is unchanged and ready.
    for ev in [Ev::Stale, Ev::Unknown] {
        for im in [Im::Unchanged, Im::Impacted] {
            for prereq in [True, False, Unknown, Absent] {
                let expected = ev == Ev::Stale && im == Im::Unchanged && prereq == True;
                let why = if expected {
                    "the change does not touch this capability and prerequisites hold"
                } else if im == Im::Impacted {
                    "the change affects this capability"
                } else if ev == Ev::Unknown {
                    "no evidence, and readiness alone does not justify continuing"
                } else {
                    match prereq {
                        False => "prerequisites are not met",
                        Unknown => "whether prerequisites hold is explicitly unknown",
                        _ => "prerequisites are not stated",
                    }
                };
                push(
                    Family::Readiness,
                    format!(
                        "readiness/{}.{}.prereq-{}",
                        ev_name(ev),
                        im_name(im),
                        slot_name(prereq)
                    ),
                    ev,
                    im,
                    vec![("prerequisites_met", prereq)],
                    expected,
                    why,
                    true,
                    false,
                );
            }
        }
    }

    // Outcome: under stale evidence with ready prerequisites, the last outcome decides.
    for im in [Im::Unchanged, Im::Impacted] {
        for outcome in [Text("success"), Text("failure"), Unknown] {
            let expected = im == Im::Unchanged && outcome == Text("success");
            let why = match (im, outcome) {
                (Im::Impacted, _) => "the change affects this capability",
                (_, Text("success")) => "an irrelevant change after a successful outcome",
                (_, Text("failure")) => "a previous failure must not be silently carried forward",
                _ => "the last outcome is explicitly unknown, so the history cannot be trusted",
            };
            push(
                Family::Outcome,
                format!("outcome/stale.{}.last-{}", im_name(im), slot_name(outcome)),
                Ev::Stale,
                im,
                vec![("prerequisites_met", True), ("last_outcome", outcome)],
                expected,
                why,
                true,
                false,
            );
        }
    }

    // Approval: any approval the operation requires must have been granted.
    for im in [Im::Unchanged, Im::Impacted] {
        for req in [True, False, Absent] {
            for grant in [True, False, Unknown, Absent, Text("conflict")] {
                if req == Absent && grant == Absent {
                    continue; // identical to readiness/stale.<im>.prereq-true
                }
                let blocked = matches!(grant, False | Unknown | Text("conflict"))
                    || (req == True && grant == Absent);
                let expected = im == Im::Unchanged && !blocked;
                let why = if im == Im::Impacted {
                    "the change affects this capability"
                } else if expected {
                    "no approval is outstanding"
                } else {
                    match grant {
                        False => "approval was refused",
                        Unknown => "whether approval was granted is explicitly unknown",
                        Text("conflict") => "approval is both granted and revoked",
                        _ => "approval is required and has not been given",
                    }
                };
                let mut facts = vec![("prerequisites_met", True), ("requires_approval", req)];
                match grant {
                    Text("conflict") => {
                        facts.push(("approval_granted", True));
                        facts.push(("approval_revoked", True));
                    }
                    other => facts.push(("approval_granted", other)),
                }
                let conflict = grant == Text("conflict");
                push(
                    Family::Approval,
                    format!(
                        "approval/stale.{}.req-{}.grant-{}",
                        im_name(im),
                        slot_name(req),
                        slot_name(grant)
                    ),
                    Ev::Stale,
                    im,
                    facts,
                    expected,
                    why,
                    true,
                    conflict,
                );
            }
        }
    }

    // Operation: unknown evidence continues only for a read-only, idempotent, ready operation.
    for im in [Im::Unchanged, Im::Impacted] {
        for ro in [True, False, Unknown, Absent] {
            for idem in [True, False, Unknown, Absent] {
                if ro == Absent && idem == Absent {
                    continue; // identical to readiness/unknown.<im>.prereq-true
                }
                let expected = im == Im::Unchanged && ro == True && idem == True;
                let why = if im == Im::Impacted {
                    "the change affects this capability"
                } else if expected {
                    "no evidence, but the operation is read-only, idempotent and ready"
                } else if ro != True {
                    match ro {
                        False => "the operation is not read-only",
                        Unknown => "whether the operation is read-only is explicitly unknown",
                        _ => "read-only is not stated",
                    }
                } else {
                    match idem {
                        False => "the operation is not idempotent",
                        Unknown => "whether the operation is idempotent is explicitly unknown",
                        _ => "idempotence is not stated",
                    }
                };
                push(
                    Family::Operation,
                    format!(
                        "operation/unknown.{}.ro-{}.idem-{}",
                        im_name(im),
                        slot_name(ro),
                        slot_name(idem)
                    ),
                    Ev::Unknown,
                    im,
                    vec![
                        ("prerequisites_met", True),
                        ("read_only", ro),
                        ("idempotent", idem),
                    ],
                    expected,
                    why,
                    true,
                    false,
                );
            }
        }
    }

    // Conflicts: a continuable situation plus one contradiction or blocking statement.
    let blockers: [(&'static str, Slot, &str); 5] = [
        ("sources_agree", False, "the sources of the facts disagree"),
        (
            "reported_failure",
            True,
            "a failure is reported against otherwise continuable facts",
        ),
        (
            "state_changed",
            True,
            "state is reported as changed yet the change is said not to matter",
        ),
        (
            "escalate_requested",
            True,
            "escalation was explicitly requested",
        ),
        (
            "policy_ambiguous",
            True,
            "the applicable policy is ambiguous",
        ),
    ];
    for (name, value, why) in blockers {
        push(
            Family::Conflict,
            format!("conflict/stale.unchanged.{name}"),
            Ev::Stale,
            Im::Unchanged,
            vec![("prerequisites_met", True), (name, value)],
            false,
            why,
            true,
            true,
        );
        push(
            Family::Conflict,
            format!("conflict/unknown.unchanged.{name}"),
            Ev::Unknown,
            Im::Unchanged,
            vec![
                ("prerequisites_met", True),
                ("read_only", True),
                ("idempotent", True),
                (name, value),
            ],
            false,
            why,
            true,
            true,
        );
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Deterministic randomness
// ---------------------------------------------------------------------------------------------

/// SplitMix64: tiny and fully specified, so generation never depends on a library.
struct Rng(u64);

impl Rng {
    fn new(seed: u64, label: &str) -> Rng {
        let mut h = Sha256::new();
        h.update(seed.to_le_bytes());
        h.update(label.as_bytes());
        let digest = h.finalize();
        Rng(u64::from_le_bytes(digest[..8].try_into().unwrap()))
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i + 1);
            items.swap(i, j);
        }
    }
}

/// A small finite pool of facts that mean nothing to any decision. Each is present with
/// probability one half, with a value from its pool, independently of the label.
fn irrelevant_facts(rng: &mut Rng) -> BTreeMap<String, FactValue> {
    let mut facts = BTreeMap::new();
    if rng.below(2) == 0 {
        let v = ["debug", "release", "staging"][rng.below(3)];
        facts.insert("target".to_string(), FactValue::Text(v.to_string()));
    }
    if rng.below(2) == 0 {
        facts.insert("retries".to_string(), FactValue::Int(rng.below(3) as i64));
    }
    if rng.below(2) == 0 {
        facts.insert("dry_run".to_string(), FactValue::Bool(rng.below(2) == 0));
    }
    if rng.below(2) == 0 {
        let v = ["low", "normal", "high"][rng.below(3)];
        facts.insert("priority".to_string(), FactValue::Text(v.to_string()));
    }
    facts
}

fn context_key(capability: &str, irrelevant: &BTreeMap<String, FactValue>) -> String {
    let facts: Vec<String> = irrelevant
        .iter()
        .map(|(k, v)| format!("{k}={}", fact_text(v)))
        .collect();
    format!("{capability}|{}", facts.join(","))
}

fn fact_text(v: &FactValue) -> String {
    match v {
        FactValue::Bool(b) => b.to_string(),
        FactValue::Int(i) => i.to_string(),
        FactValue::Text(t) => t.clone(),
    }
}

/// The graph token of a context: a function of the context key and the seed only.
fn graph_state(seed: u64, key: &str, ordinal: usize) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"chip.decision-corpus.graph-state");
    h.update(seed.to_le_bytes());
    h.update(key.as_bytes());
    h.update((ordinal as u64).to_le_bytes());
    h.finalize().into()
}

fn stable_hash(seed: u64, label: &str) -> u64 {
    Rng::new(seed, label).next()
}

// ---------------------------------------------------------------------------------------------
// Generation
// ---------------------------------------------------------------------------------------------

fn pattern_facts(p: &Pattern, rng: &mut Rng) -> BTreeMap<String, FactValue> {
    let mut facts = BTreeMap::new();
    for (name, slot) in &p.facts {
        let value = match slot {
            Slot::Absent => continue,
            Slot::True => FactValue::Bool(true),
            Slot::False => FactValue::Bool(false),
            Slot::Unknown => {
                FactValue::Text(UNKNOWN_SPELLINGS[rng.below(UNKNOWN_SPELLINGS.len())].to_string())
            }
            // The "conflict" pseudo-slot is expanded by the pattern itself.
            Slot::Text(t) => FactValue::Text((*t).to_string()),
        };
        facts.insert((*name).to_string(), value);
    }
    facts
}

/// The decision-relevant description of a pattern, for measuring how close two patterns are.
fn fingerprint(p: &Pattern) -> BTreeMap<String, String> {
    let mut f = BTreeMap::new();
    f.insert("#evidence".to_string(), ev_name(p.evidence).to_string());
    f.insert("#impact".to_string(), im_name(p.impact).to_string());
    for (name, slot) in &p.facts {
        if *slot != Slot::Absent {
            f.insert((*name).to_string(), slot_name(*slot).to_string());
        }
    }
    f
}

fn distance(a: &BTreeMap<String, String>, b: &BTreeMap<String, String>) -> usize {
    let keys: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    keys.into_iter().filter(|k| a.get(*k) != b.get(*k)).count()
}

pub fn generate(config: &GeneratorConfig) -> Corpus {
    let patterns = patterns();
    let prints: Vec<_> = patterns.iter().map(fingerprint).collect();
    let positives: Vec<usize> = (0..patterns.len())
        .filter(|&i| patterns[i].expected_continue)
        .collect();

    // A negative is "hard" when some positive differs from it in exactly one decision factor.
    let hard: Vec<bool> = (0..patterns.len())
        .map(|i| {
            !patterns[i].expected_continue
                && positives
                    .iter()
                    .any(|&j| distance(&prints[i], &prints[j]) == 1)
        })
        .collect();

    // Which patterns are withheld: a seeded, stratified quarter, chosen so that every fact the
    // withheld patterns use is still seen in training (a novel *combination*, not a novel fact).
    let held_patterns = choose_held_out_patterns(config, &patterns);

    // Raw cases.
    struct Raw {
        pattern: usize,
        capability: &'static str,
        irrelevant: BTreeMap<String, FactValue>,
        facts: BTreeMap<String, FactValue>,
        ordinal: usize,
    }
    let mut raw: Vec<Raw> = Vec::new();
    for (index, p) in patterns.iter().enumerate() {
        let variants = if !p.expected_continue {
            config.variants_per_negative
        } else if p.evidence == Ev::Valid {
            config.variants_per_baseline_positive
        } else {
            config.variants_per_target_positive
        };
        let mut rng = Rng::new(config.seed, &format!("pattern:{}", p.id));
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut attempts = 0;
        while seen.len() < variants {
            attempts += 1;
            assert!(
                attempts < 10_000,
                "cannot find enough distinct contexts for {}",
                p.id
            );
            let capability = CAPABILITIES[rng.below(CAPABILITIES.len())];
            let irrelevant = irrelevant_facts(&mut rng);
            let key = context_key(capability, &irrelevant);
            let facts = pattern_facts(p, &mut rng);
            // Distinct states only: the unknown spelling is part of the state.
            let mut all = facts.clone();
            all.extend(irrelevant.clone());
            let state_key = format!(
                "{key}|{}",
                all.iter()
                    .map(|(k, v)| format!("{k}={}", fact_text(v)))
                    .collect::<Vec<_>>()
                    .join(",")
            );
            if seen.insert(state_key) {
                raw.push(Raw {
                    pattern: index,
                    capability,
                    irrelevant,
                    facts,
                    ordinal: seen.len(),
                });
            }
        }
    }

    // Assign ids in a seeded shuffled order, so neither position nor id says anything about the
    // pattern or the label.
    let mut order: Vec<usize> = (0..raw.len()).collect();
    Rng::new(config.seed, "case-order").shuffle(&mut order);

    // Context holdout: a fifth of the (capability, irrelevant facts) combinations, by hash.
    let held_context =
        |key: &str| stable_hash(config.seed, &format!("context-holdout:{key}")) % 5 == 0;
    let held_capability = |c: &str| HELD_OUT_CAPABILITIES.contains(&c);

    let mut cases: Vec<CorpusCase> = Vec::with_capacity(raw.len());
    for (position, &r) in order.iter().enumerate() {
        let r = &raw[r];
        let p = &patterns[r.pattern];
        let key = context_key(r.capability, &r.irrelevant);
        let mut facts = r.facts.clone();
        facts.extend(r.irrelevant.clone());
        let tags = Tags {
            hard_negative: hard[r.pattern],
            conjunction: p.conjunction,
            conflict: p.conflict,
            unknown: facts.values().any(FactValue::is_unknown)
                && !r.irrelevant.values().any(FactValue::is_unknown),
        };
        let mut holdouts = Holdouts {
            case: false,
            pattern: held_patterns.contains(&r.pattern),
            context: !held_capability(r.capability) && held_context(&key),
            capability: held_capability(r.capability),
        };
        let eligible = !holdouts.pattern && !holdouts.context && !holdouts.capability;
        // Case holdout and validation: random draws among what remains.
        let draw = stable_hash(config.seed, &format!("case-split:{position}")) % 100;
        let mut split = Split::Train;
        if eligible && draw < 10 {
            holdouts.case = true;
        } else if eligible && draw < 18 {
            split = Split::Validation;
        }
        if holdouts.any() {
            split = Split::HeldOut;
        }
        cases.push(CorpusCase {
            case_id: format!("dc-{:04}", position + 1),
            family: p.family,
            pattern_id: p.id.clone(),
            expected_continue: p.expected_continue,
            evidence: match p.evidence {
                Ev::Valid => EvidenceState::KnownValid,
                Ev::Stale => EvidenceState::KnownStale,
                Ev::Unknown => EvidenceState::Unknown,
            },
            impact: match p.impact {
                Im::Unchanged => ImpactState::Unchanged,
                Im::Impacted => ImpactState::Impacted,
            },
            capability: r.capability.to_string(),
            graph_state: graph_state(config.seed, &key, r.ordinal),
            facts,
            rationale: p.rationale.clone(),
            context_id: key,
            split,
            holdouts,
            tags,
        });
    }
    Corpus {
        generator: GENERATOR_VERSION,
        config: *config,
        cases,
    }
}

fn choose_held_out_patterns(config: &GeneratorConfig, patterns: &[Pattern]) -> BTreeSet<usize> {
    // Strata: target positives, baseline positives, then negatives by family.
    let mut strata: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, p) in patterns.iter().enumerate() {
        let stratum = if p.expected_continue && p.evidence == Ev::Valid {
            "baseline-positive".to_string()
        } else if p.expected_continue {
            "target-positive".to_string()
        } else {
            format!("negative/{}", p.family.name())
        };
        strata.entry(stratum).or_default().push(i);
    }
    let mut chosen: BTreeSet<usize> = BTreeSet::new();
    for (stratum, mut members) in strata {
        Rng::new(config.seed, &format!("pattern-holdout:{stratum}")).shuffle(&mut members);
        let quota = match stratum.as_str() {
            "target-positive" => 2,
            "baseline-positive" => 1,
            _ => (members.len() as f64 * 0.25).round().max(1.0) as usize,
        };
        // Keep a pattern only if every fact it uses is still used by a pattern that stays in
        // training.
        for &candidate in &members {
            if chosen
                .iter()
                .filter(|&&c| stratum_of(&patterns[c]) == stratum)
                .count()
                >= quota
            {
                break;
            }
            chosen.insert(candidate);
            if !supported(patterns, &chosen) {
                chosen.remove(&candidate);
            }
        }
    }
    chosen
}

fn stratum_of(p: &Pattern) -> String {
    if p.expected_continue && p.evidence == Ev::Valid {
        "baseline-positive".to_string()
    } else if p.expected_continue {
        "target-positive".to_string()
    } else {
        format!("negative/{}", p.family.name())
    }
}

/// Every (fact, slot) of every withheld pattern, and its evidence and impact, still occurs in a
/// pattern that is kept for training.
fn supported(patterns: &[Pattern], held: &BTreeSet<usize>) -> bool {
    let kept: BTreeSet<(String, String)> = patterns
        .iter()
        .enumerate()
        .filter(|(i, _)| !held.contains(i))
        .flat_map(|(_, p)| fingerprint(p).into_iter())
        .collect();
    held.iter().all(|&i| {
        fingerprint(&patterns[i])
            .into_iter()
            .all(|kv| kept.contains(&kv))
    })
}

// ---------------------------------------------------------------------------------------------
// Canonical text
// ---------------------------------------------------------------------------------------------

fn json_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn evidence_name(e: EvidenceState) -> &'static str {
    e.wire_name()
}

impl Corpus {
    /// The corpus as JSON Lines: a header, then one case per line, in a fixed field order.
    /// This is the committed artifact; byte-identical for identical generator and config.
    pub fn to_jsonl(&self) -> String {
        let mut out = format!(
            "{{\"schema\":\"{CORPUS_SCHEMA}\",\"generator\":\"{}\",\"seed\":{},\"variants\":[{},{},{}],\"cases\":{}}}\n",
            self.generator,
            self.config.seed,
            self.config.variants_per_target_positive,
            self.config.variants_per_baseline_positive,
            self.config.variants_per_negative,
            self.cases.len()
        );
        for c in &self.cases {
            let facts: Vec<String> = c
                .facts
                .iter()
                .map(|(k, v)| {
                    let value = match v {
                        FactValue::Bool(b) => b.to_string(),
                        FactValue::Int(i) => i.to_string(),
                        FactValue::Text(t) => json_string(t),
                    };
                    format!("{}:{}", json_string(k), value)
                })
                .collect();
            let hex: String = c.graph_state.iter().map(|b| format!("{b:02x}")).collect();
            out.push_str(&format!(
                "{{\"case_id\":{},\"family\":{},\"pattern_id\":{},\"expected\":\"{}\",\"evidence\":\"{}\",\"impact\":\"{}\",\"capability\":{},\"graph_state\":\"sha256:{hex}\",\"facts\":{{{}}},\"context_id\":{},\"split\":\"{}\",\"holdout\":{{\"case\":{},\"pattern\":{},\"context\":{},\"capability\":{}}},\"tags\":{{\"hard_negative\":{},\"conjunction\":{},\"conflict\":{},\"unknown\":{}}},\"rationale\":{}}}\n",
                json_string(&c.case_id),
                json_string(c.family.name()),
                json_string(&c.pattern_id),
                if c.expected_continue { "continue" } else { "escalate" },
                evidence_name(c.evidence),
                c.impact.wire_name(),
                json_string(&c.capability),
                facts.join(","),
                json_string(&c.context_id),
                c.split.name(),
                c.holdouts.case,
                c.holdouts.pattern,
                c.holdouts.context,
                c.holdouts.capability,
                c.tags.hard_negative,
                c.tags.conjunction,
                c.tags.conflict,
                c.tags.unknown,
                json_string(&c.rationale),
            ));
        }
        out
    }

    /// SHA-256 of [`Corpus::to_jsonl`].
    pub fn digest(&self) -> String {
        let hash = Sha256::digest(self.to_jsonl().as_bytes());
        hash.iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// The corpus the repository commits: generator version 1 with its default configuration.
pub fn corpus_v1() -> Corpus {
    generate(&GeneratorConfig::V1)
}
