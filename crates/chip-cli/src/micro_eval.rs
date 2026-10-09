//! Evaluation of shadow-mode micro-model nominations against a labelled fixture.
//!
//! Not part of the product path: nothing here runs during `chip work`. It scores what a model said
//! about fixed snapshots, against labels that were fixed beforehand and that no model produced (see
//! `tests/fixtures/micro/build.py`). Scoring is a pure function of the fixture and the replies, so a
//! recorded run can be replayed to the same metrics.
//!
//! Measurements that cannot be made are `null` with a reason, never zero. A run with no model is
//! `blocked` and reports no metrics. A scripted responder exists to test the harness and is labelled
//! `scripted_self_test` everywhere it appears; its numbers say nothing about any model.

use std::collections::BTreeMap;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::micro::{
    Applicability, Attempt, Budgets, EvidenceItem, Expected, FailureClass, MicroResponse,
    Nomination, Outcome, RejectCode, Snapshot, StrategyId, validate,
};

pub const FIXTURE_PATH: &str = "tests/fixtures/micro/fixture.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Split {
    Calibration,
    Heldout,
}

impl Split {
    pub fn name(self) -> &'static str {
        match self {
            Split::Calibration => "calibration",
            Split::Heldout => "heldout",
        }
    }
}

/// What a reviewer accepts for a case. Fixed before any model is run; never derived from a reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    pub classes: Vec<FailureClass>,
    pub strategies: Vec<StrategyId>,
    pub must_abstain: bool,
    pub label_source: String,
    pub human_review: String,
}

#[derive(Debug, Clone)]
pub struct Case {
    pub id: String,
    pub split: Split,
    pub category: String,
    pub source: String,
    /// `initial-harness` (the first 22, kept as harness fixtures) or `expansion`.
    pub cohort: String,
    /// `executed_reproduced_and_verified` (a native tool was run, the failure reproduced identically and
    /// the labelled correction made PAX pass) or `unexecuted_synthetic` (hand-written; nothing was run).
    pub execution_status: String,
    /// The case embeds the repository files needed to reproduce it.
    pub reproducible: bool,
    pub snapshot: Snapshot,
    pub label: Label,
}

impl Case {
    pub fn executed(&self) -> bool {
        self.execution_status == "executed_reproduced_and_verified"
    }
}

#[derive(Debug, Clone)]
pub struct Fixture {
    pub version: String,
    /// SHA-256 of the fixture file as read.
    pub sha256: String,
    /// SHA-256 over the canonical JSON of every held-out case, in id order. Compared with the frozen value.
    pub heldout_sha256: String,
    pub cases: Vec<Case>,
}

const LABEL_SOURCES: &[&str] = &[
    "constructed_defect_fix_verified_by_pax",
    "synthetic_unverified",
];

fn text<'a>(v: &'a serde_json::Value, key: &str) -> Result<&'a str, String> {
    v[key]
        .as_str()
        .ok_or_else(|| format!("`{key}` must be a string"))
}

fn list<T>(
    v: &serde_json::Value,
    key: &str,
    parse: impl Fn(&str) -> Option<T>,
) -> Result<Vec<T>, String> {
    v[key]
        .as_array()
        .ok_or_else(|| format!("`{key}` must be an array"))?
        .iter()
        .map(|e| {
            let s = e
                .as_str()
                .ok_or_else(|| format!("`{key}` holds a non-string"))?;
            parse(s).ok_or_else(|| format!("`{key}` holds `{s}`, which is not in the closed set"))
        })
        .collect()
}

fn parse_case(v: &serde_json::Value) -> Result<Case, String> {
    let id = text(v, "id")?.to_string();
    let wrap = |e: String| format!("case {id}: {e}");
    let split = match text(v, "split").map_err(wrap)? {
        "calibration" => Split::Calibration,
        "heldout" => Split::Heldout,
        other => return Err(format!("case {id}: unknown split `{other}`")),
    };
    let s = &v["snapshot"];
    let evidence = s["evidence"]
        .as_array()
        .ok_or_else(|| format!("case {id}: `evidence` must be an array"))?
        .iter()
        .map(|e| {
            Ok(EvidenceItem {
                id: text(e, "id")?.to_string(),
                capability: text(e, "capability")?.to_string(),
                fresh: e["fresh"].as_bool().ok_or("`fresh` must be a boolean")?,
                paths: e["paths"]
                    .as_array()
                    .ok_or("`paths` must be an array")?
                    .iter()
                    .map(|p| {
                        p.as_str()
                            .map(str::to_string)
                            .ok_or("a path must be a string")
                    })
                    .collect::<Result<_, _>>()?,
                excerpt: text(e, "excerpt")?.to_string(),
            })
        })
        .collect::<Result<Vec<_>, String>>()
        .map_err(|e| format!("case {id}: {e}"))?;
    let snapshot = Snapshot {
        contract_version: crate::micro::INTERIM_CONTRACT_VERSION,
        contract_digest: "sha256:fixture".into(),
        pax_status: text(s, "pax_status").map_err(wrap)?.to_string(),
        pax_reason: text(s, "pax_reason").map_err(wrap)?.to_string(),
        exit_code: s["exit_code"].as_i64(),
        diagnostics: text(s, "diagnostics").map_err(wrap)?.to_string(),
        diagnostics_truncated: s["diagnostics_truncated"].as_bool().unwrap_or(false),
        evidence,
        candidates: list(s, "candidates", StrategyId::parse).map_err(wrap)?,
        budgets: Budgets {
            turns_remaining: s["budgets"]["turns_remaining"].as_u64().unwrap_or(0),
            executions_remaining: s["budgets"]["executions_remaining"].as_u64().unwrap_or(0),
        },
    };
    let l = &v["label"];
    let label = Label {
        classes: list(l, "acceptable_classes", FailureClass::parse).map_err(wrap)?,
        strategies: list(l, "acceptable_strategies", StrategyId::parse).map_err(wrap)?,
        must_abstain: l["must_abstain"]
            .as_bool()
            .ok_or_else(|| format!("case {id}: `must_abstain` must be a boolean"))?,
        label_source: text(l, "label_source").map_err(wrap)?.to_string(),
        human_review: text(l, "human_review").map_err(wrap)?.to_string(),
    };
    Ok(Case {
        category: text(v, "category").map_err(wrap)?.to_string(),
        source: text(v, "source").map_err(wrap)?.to_string(),
        cohort: text(v, "cohort").map_err(wrap)?.to_string(),
        execution_status: text(v, "execution_status").map_err(wrap)?.to_string(),
        reproducible: v["repository"]["files"]
            .as_object()
            .is_some_and(|f| !f.is_empty())
            && v["repository"]["tree_sha256"].is_string(),
        id,
        split,
        snapshot,
        label,
    })
}

/// Reads a fixture. Rejects anything malformed; a fixture that does not parse is never partly used.
pub fn load(raw: &str) -> Result<Fixture, String> {
    let v: serde_json::Value = serde_json::from_str(raw).map_err(|e| format!("fixture: {e}"))?;
    if v["schema"] != crate::micro::SCHEMA {
        return Err(format!(
            "fixture: schema must be `{}`",
            crate::micro::SCHEMA
        ));
    }
    let cases = v["cases"]
        .as_array()
        .ok_or("fixture: `cases` must be an array")?
        .iter()
        .map(parse_case)
        .collect::<Result<Vec<_>, _>>()?;
    let mut heldout: Vec<&serde_json::Value> = v["cases"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["split"] == "heldout")
        .collect();
    heldout.sort_by_key(|c| c["id"].as_str().unwrap_or_default().to_string());
    let canonical: String = heldout
        .iter()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    Ok(Fixture {
        heldout_sha256: format!("sha256:{}", hex(&Sha256::digest(canonical.as_bytes()))),
        version: text(&v, "fixture_version")?.to_string(),
        sha256: format!("sha256:{}", hex(&Sha256::digest(raw.as_bytes()))),
        cases,
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Debug, Clone)]
pub struct Check {
    pub name: &'static str,
    pub passed: bool,
    pub detail: String,
}

/// The frozen held-out set: its case ids and content hash, recorded before any candidate model was evaluated.
pub const FREEZE_PATH: &str = "tests/fixtures/micro/heldout.freeze.json";

/// The freeze document for this fixture. Written once, by `micro_eval --freeze`, and never overwritten.
pub fn freeze_document(f: &Fixture, commit: Option<String>) -> serde_json::Value {
    let mut ids: Vec<&str> = f
        .cases
        .iter()
        .filter(|c| c.split == Split::Heldout)
        .map(|c| c.id.as_str())
        .collect();
    ids.sort_unstable();
    serde_json::json!({
        "fixture_version": f.version,
        "heldout_ids": ids,
        "heldout_sha256": f.heldout_sha256,
        "frozen_at_commit": commit,
        "rule": "Frozen before any candidate model was evaluated. Changing a held-out case, adding one, or moving one between splits requires a new fixture version and a new freeze; a run against a fixture whose held-out hash differs from this file is flagged and is not held-out evidence.",
    })
}

/// Whether the held-out set is the one that was frozen.
pub fn frozen_check(f: &Fixture, freeze_raw: Option<&str>) -> Check {
    let name = "held-out set matches its freeze";
    let Some(raw) = freeze_raw else {
        return Check {
            name,
            passed: false,
            detail: "no freeze file".into(),
        };
    };
    let Ok(freeze) = serde_json::from_str::<serde_json::Value>(raw) else {
        return Check {
            name,
            passed: false,
            detail: "the freeze file does not parse".into(),
        };
    };
    let now = freeze_document(f, None);
    let same = freeze["fixture_version"] == now["fixture_version"]
        && freeze["heldout_ids"] == now["heldout_ids"]
        && freeze["heldout_sha256"] == now["heldout_sha256"];
    Check {
        name,
        passed: same,
        detail: if same {
            "ok".into()
        } else {
            "the held-out cases differ from the frozen set".into()
        },
    }
}

/// Structural checks on the fixture itself: the properties the evaluation relies on.
pub fn fixture_checks(f: &Fixture) -> Vec<Check> {
    let mut out = Vec::new();
    let mut check = |name: &'static str, bad: Vec<String>| {
        out.push(Check {
            name,
            passed: bad.is_empty(),
            detail: if bad.is_empty() {
                "ok".into()
            } else {
                bad.join("; ")
            },
        });
    };
    let mut seen = std::collections::BTreeSet::new();
    check(
        "case ids are unique",
        f.cases
            .iter()
            .filter(|c| !seen.insert(c.id.clone()))
            .map(|c| c.id.clone())
            .collect(),
    );
    let count = |s: Split| f.cases.iter().filter(|c| c.split == s).count();
    check(
        "both splits are non-empty",
        [Split::Calibration, Split::Heldout]
            .into_iter()
            .filter(|s| count(*s) == 0)
            .map(|s| s.name().to_string())
            .collect(),
    );
    check(
        "label sources are independent of any model",
        f.cases
            .iter()
            .filter(|c| !LABEL_SOURCES.contains(&c.label.label_source.as_str()))
            .map(|c| c.id.clone())
            .collect(),
    );
    check(
        "every label is marked for human review",
        f.cases
            .iter()
            .filter(|c| c.label.human_review != "pending" && c.label.human_review != "reviewed")
            .map(|c| c.id.clone())
            .collect(),
    );
    check(
        "acceptable strategies are offered candidates",
        f.cases
            .iter()
            .filter(|c| {
                !c.label.must_abstain
                    && c.label
                        .strategies
                        .iter()
                        .any(|s| !c.snapshot.candidates.contains(s))
            })
            .map(|c| c.id.clone())
            .collect(),
    );
    check(
        "abstention cases accept only the unknown class",
        f.cases
            .iter()
            .filter(|c| c.label.must_abstain && c.label.classes != [FailureClass::Unknown])
            .map(|c| c.id.clone())
            .collect(),
    );
    check(
        "executed and unexecuted cases are distinguished",
        f.cases
            .iter()
            .filter(|c| {
                let native = c.source == "native_capture";
                (native && (!c.executed() || !c.reproducible))
                    || (!native && (c.executed() || c.source != "synthetic"))
            })
            .map(|c| c.id.clone())
            .collect(),
    );
    check(
        "enough executed cases per split to compute an interval",
        [Split::Calibration, Split::Heldout]
            .into_iter()
            .filter(|s| {
                f.cases
                    .iter()
                    .filter(|c| c.split == *s && c.executed())
                    .count()
                    < 10
            })
            .map(|s| s.name().to_string())
            .collect(),
    );
    check(
        "stale evidence appears in the fixture",
        if f.cases
            .iter()
            .any(|c| c.snapshot.evidence.iter().any(|e| !e.fresh))
        {
            vec![]
        } else {
            vec!["no stale evidence case".into()]
        },
    );
    for category in [
        "familiar",
        "unfamiliar",
        "ambiguous",
        "stale_evidence",
        "invalid_strategy_offered",
        "adversarial",
    ] {
        let name: &'static str = match category {
            "familiar" => "category present: familiar",
            "unfamiliar" => "category present: unfamiliar",
            "ambiguous" => "category present: ambiguous",
            "stale_evidence" => "category present: stale_evidence",
            "invalid_strategy_offered" => "category present: invalid_strategy_offered",
            "missing_evidence" => "category present: missing_evidence",
            "repeated_failure" => "category present: repeated_failure",
            "no_strategy_applicable" => "category present: no_strategy_applicable",
            _ => "category present: adversarial",
        };
        check(
            name,
            if f.cases.iter().any(|c| c.category == category) {
                vec![]
            } else {
                vec![format!("no `{category}` case")]
            },
        );
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Responders: how an attempt is obtained for a snapshot.
// ---------------------------------------------------------------------------------------------

#[async_trait::async_trait]
pub trait Responder: Send + Sync {
    async fn ask(&self, case: &Case, index: usize) -> Attempt;
}

/// A real model behind FX, asked exactly as shadow mode asks it.
pub struct ProviderResponder {
    pub provider: std::sync::Arc<dyn fx_core::ModelProvider>,
    pub model: String,
    pub timeout: Duration,
}

#[async_trait::async_trait]
impl Responder for ProviderResponder {
    async fn ask(&self, case: &Case, _index: usize) -> Attempt {
        crate::micro::nominate(
            self.provider.as_ref(),
            &self.model,
            &case.snapshot,
            self.timeout,
        )
        .await
    }
}

fn attempt_from_reply(case: &Case, reply: &str) -> Attempt {
    let nomination = match validate(reply, &Expected::new(&case.snapshot)) {
        Ok(v) => Nomination::Valid(v),
        Err(r) => Nomination::Rejected(r),
    };
    Attempt {
        nomination,
        reply: Some(reply.to_string()),
        latency: Duration::ZERO,
        prompt_tokens: None,
        completion_tokens: None,
    }
}

/// Replies recorded earlier, replayed: the same fixture and replies score to the same metrics.
pub struct Replay(pub BTreeMap<String, String>);

#[async_trait::async_trait]
impl Responder for Replay {
    async fn ask(&self, case: &Case, _index: usize) -> Attempt {
        match self.0.get(&case.id) {
            Some(reply) => attempt_from_reply(case, reply),
            None => Attempt {
                nomination: Nomination::ProviderFailed("no recorded reply".into()),
                reply: None,
                latency: Duration::ZERO,
                prompt_tokens: None,
                completion_tokens: None,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Script {
    /// Answers every case with the label's own answer. A ceiling for the harness, not a model.
    Oracle,
    /// Abstains on every case.
    AbstainAll,
    /// Cycles through every way a reply can be wrong.
    Adversarial,
}

pub struct Scripted(pub Script);

fn reply_json(case: &Case, fields: &[(&str, serde_json::Value)]) -> String {
    let mut m = serde_json::Map::new();
    m.insert("schema".into(), crate::micro::SCHEMA.into());
    m.insert(
        "contract_version".into(),
        case.snapshot.contract_version.into(),
    );
    m.insert("snapshot_id".into(), case.snapshot.id().into());
    m.insert("relevant_scope".into(), serde_json::json!([]));
    for (k, v) in fields {
        m.insert((*k).into(), v.clone());
    }
    serde_json::Value::Object(m).to_string()
}

fn abstain_reply(case: &Case) -> String {
    let reason = match case.category.as_str() {
        "ambiguous" => "ambiguous_diagnostics",
        "unfamiliar" => "unfamiliar_failure",
        "stale_evidence" => "stale_evidence",
        _ => "insufficient_evidence",
    };
    reply_json(
        case,
        &[("outcome", "abstained".into()), ("reason", reason.into())],
    )
}

fn oracle_reply(case: &Case) -> String {
    if case.label.must_abstain {
        return abstain_reply(case);
    }
    let class = case
        .label
        .classes
        .iter()
        .find(|c| **c != FailureClass::Unknown)
        .copied()
        .unwrap_or(FailureClass::Unknown);
    let mut fields: Vec<(&str, serde_json::Value)> = vec![
        ("outcome", "classified".into()),
        ("classification", class.as_str().into()),
    ];
    if let Some(s) = case.label.strategies.first() {
        fields.push(("strategy_id", s.as_str().into()));
        fields.push(("applicability", "applicable".into()));
    }
    if let Some((item, path)) = case
        .snapshot
        .fresh_evidence()
        .find_map(|e| e.paths.first().map(|p| (e, p)))
    {
        fields.push((
            "relevant_scope",
            serde_json::json!([{"path": path, "evidence_id": item.id}]),
        ));
    }
    reply_json(case, &fields)
}

fn adversarial_reply(case: &Case, index: usize) -> Option<String> {
    let good = oracle_reply(case);
    let with = |from: &str, to: &str| good.replacen(from, to, 1);
    let stale = case.snapshot.evidence.iter().find(|e| !e.fresh);
    Some(match index % 8 {
        0 => format!("I think the answer is: {good}"),
        1 => match stale {
            Some(e) => reply_json(
                case,
                &[
                    ("outcome", "classified".into()),
                    ("classification", "test_assertion_failure".into()),
                    (
                        "relevant_scope",
                        serde_json::json!([{"path": e.paths.first().cloned().unwrap_or_default(), "evidence_id": e.id}]),
                    ),
                ],
            ),
            None => reply_json(
                case,
                &[
                    ("outcome", "classified".into()),
                    ("classification", "unknown".into()),
                    (
                        "relevant_scope",
                        serde_json::json!([{"path": "src/lib.rs", "evidence_id": "ev-99"}]),
                    ),
                ],
            ),
        },
        2 => reply_json(
            case,
            &[
                ("outcome", "classified".into()),
                ("classification", "compile_error".into()),
                ("strategy_id", "rewrite_everything".into()),
                ("applicability", "applicable".into()),
            ],
        ),
        3 => with(&case.snapshot.id(), "snap-0000000000000000"),
        4 => with("\"contract_version\":0", "\"contract_version\":9"),
        5 => with('{'.to_string().as_str(), "{\"execute\":\"rm -rf /\","),
        6 => reply_json(
            case,
            &[
                ("outcome", "classified".into()),
                ("classification", "environment_or_permission".into()),
                (
                    "strategy_id",
                    case.snapshot.candidates.first()?.as_str().into(),
                ),
                ("applicability", "applicable".into()),
            ],
        ),
        _ => return None,
    })
}

#[async_trait::async_trait]
impl Responder for Scripted {
    async fn ask(&self, case: &Case, index: usize) -> Attempt {
        let reply = match self.0 {
            Script::Oracle => oracle_reply(case),
            Script::AbstainAll => abstain_reply(case),
            Script::Adversarial => match adversarial_reply(case, index) {
                Some(r) => r,
                None => {
                    return Attempt {
                        nomination: Nomination::ProviderFailed("scripted outage".into()),
                        reply: None,
                        latency: Duration::ZERO,
                        prompt_tokens: None,
                        completion_tokens: None,
                    };
                }
            },
        };
        attempt_from_reply(case, &reply)
    }
}

// ---------------------------------------------------------------------------------------------
// Scoring.
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CaseResult {
    pub case: Case,
    pub attempt: Attempt,
}

fn valid(r: &CaseResult) -> Option<&MicroResponse> {
    match &r.attempt.nomination {
        Nomination::Valid(v) => Some(v),
        _ => None,
    }
}

/// A reply that declines to classify, or classifies as unknown without nominating anything.
fn declines(v: &MicroResponse) -> bool {
    matches!(v.outcome, Outcome::Abstained | Outcome::Blocked)
        || (v.classification == Some(FailureClass::Unknown) && v.strategy.is_none())
}

fn rate(numerator: usize, denominator: usize) -> serde_json::Value {
    if denominator == 0 {
        serde_json::Value::Null
    } else {
        serde_json::json!(numerator as f64 / denominator as f64)
    }
}

fn percentile(sorted: &[u64], p: f64) -> serde_json::Value {
    if sorted.is_empty() {
        return serde_json::Value::Null;
    }
    let at = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    serde_json::json!(sorted[at])
}

fn mean(values: &[u32]) -> serde_json::Value {
    if values.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::json!(values.iter().map(|v| *v as f64).sum::<f64>() / values.len() as f64)
    }
}

/// The 95 % Wilson score interval for `k` successes in `n` trials. With `n = 0` there is no interval.
pub fn wilson(k: usize, n: usize) -> serde_json::Value {
    if n == 0 {
        return serde_json::json!({"k": 0, "n": 0, "low": null, "high": null});
    }
    let (k, nf) = (k as f64, n as f64);
    let z = 1.959_963_984_540_054_f64;
    let p = k / nf;
    let denom = 1.0 + z * z / nf;
    let centre = (p + z * z / (2.0 * nf)) / denom;
    let half = z * ((p * (1.0 - p) / nf) + z * z / (4.0 * nf * nf)).sqrt() / denom;
    serde_json::json!({"k": k as usize, "n": n, "low": (centre - half).max(0.0), "high": (centre + half).min(1.0)})
}

/// The class a case is filed under in per-class reporting: its first acceptable class, or `unknown`
/// where abstention is required.
pub fn primary_class(c: &Case) -> &'static str {
    if c.label.must_abstain {
        return "unknown";
    }
    c.label.classes.first().map_or("unknown", |c| c.as_str())
}

const NOT_IN_SHADOW: &str = "not measurable in shadow mode: nothing changes the work; requires the controlled ablation of RIC-07";

/// The metrics for a set of results. Every rate carries its numerator and denominator; a rate whose
/// denominator is zero is `null`, never `0`.
pub fn metrics(results: &[&CaseResult]) -> serde_json::Value {
    let n = results.len();
    let replied: Vec<&&CaseResult> = results
        .iter()
        .filter(|r| r.attempt.reply.is_some())
        .collect();
    let failures = results
        .iter()
        .filter(|r| {
            matches!(
                r.attempt.nomination,
                Nomination::ProviderFailed(_) | Nomination::TimedOut
            )
        })
        .count();
    let valids: Vec<&&CaseResult> = results.iter().filter(|r| valid(r).is_some()).collect();
    let mut rejection_codes: BTreeMap<&str, usize> = BTreeMap::new();
    for r in results {
        if let Nomination::Rejected(rej) = &r.attempt.nomination {
            *rejection_codes.entry(rej.code.as_str()).or_default() += 1;
        }
    }
    let rejected = rejection_codes.values().sum::<usize>();

    // Classification. An abstention is a prediction of `unknown`.
    let correct = valids
        .iter()
        .filter(|r| {
            let v = valid(r).unwrap();
            let predicted = v.classification.unwrap_or(FailureClass::Unknown);
            r.case.label.classes.contains(&predicted)
        })
        .count();
    let classified: Vec<&&&CaseResult> = valids
        .iter()
        .filter(|r| valid(r).unwrap().outcome == Outcome::Classified)
        .collect();
    let classified_correct = classified
        .iter()
        .filter(|r| {
            r.case
                .label
                .classes
                .contains(&valid(r).unwrap().classification.unwrap())
        })
        .count();

    // Strategy nomination: only a strategy the model calls applicable (or is unsure of) counts.
    let nominations: Vec<&&&CaseResult> = valids
        .iter()
        .filter(|r| {
            let v = valid(r).unwrap();
            v.strategy.is_some() && v.applicability != Some(Applicability::NotApplicable)
        })
        .collect();
    let false_positives = nominations
        .iter()
        .filter(|r| {
            let v = valid(r).unwrap();
            r.case.label.must_abstain || !r.case.label.strategies.contains(&v.strategy.unwrap())
        })
        .count();

    // Abstention.
    let must: Vec<&&&CaseResult> = valids
        .iter()
        .filter(|r| r.case.label.must_abstain)
        .collect();
    let must_declined = must.iter().filter(|r| declines(valid(r).unwrap())).count();
    let answerable: Vec<&&&CaseResult> = valids
        .iter()
        .filter(|r| !r.case.label.must_abstain)
        .collect();
    let answerable_declined = answerable
        .iter()
        .filter(|r| declines(valid(r).unwrap()))
        .count();

    let mut latencies: Vec<u64> = replied
        .iter()
        .map(|r| r.attempt.latency.as_millis() as u64)
        .collect();
    latencies.sort_unstable();
    let prompt: Vec<u32> = results
        .iter()
        .filter_map(|r| r.attempt.prompt_tokens)
        .collect();
    let completion: Vec<u32> = results
        .iter()
        .filter_map(|r| r.attempt.completion_tokens)
        .collect();
    let timing_available = !replied.is_empty() && latencies.iter().any(|l| *l > 0);

    serde_json::json!({
        "cases": n,
        "replies_received": replied.len(),
        "provider_failures": failures,
        "valid_replies": valids.len(),
        "rejected_replies": rejected,
        "rejection_codes": rejection_codes,
        "schema_valid_rate": rate(valids.len(), replied.len()),
        "classification": {
            "accuracy_over_valid_replies": rate(correct, valids.len()),
            "correct": correct,
            "valid": valids.len(),
            "classified_rate": rate(classified.len(), valids.len()),
            "accuracy_when_classified": rate(classified_correct, classified.len()),
        },
        "strategy": {
            "nominations": nominations.len(),
            "false_positives": false_positives,
            "false_positive_rate": rate(false_positives, nominations.len()),
        },
        "abstention": {
            "cases_requiring_abstention_answered": must.len(),
            "appropriate": must_declined,
            "appropriate_abstention_rate": rate(must_declined, must.len()),
            "answerable_cases_answered": answerable.len(),
            "inappropriate_abstentions": answerable_declined,
            "inappropriate_abstention_rate": rate(answerable_declined, answerable.len()),
        },
        "confidence_95_wilson": {
            "schema_valid_rate": wilson(valids.len(), replied.len()),
            "classification_accuracy_over_valid_replies": wilson(correct, valids.len()),
            "false_positive_strategy_rate": wilson(false_positives, nominations.len()),
            "appropriate_abstention_rate": wilson(must_declined, must.len()),
            "inappropriate_abstention_rate": wilson(answerable_declined, answerable.len()),
        },
        "latency_ms": {
            "p50": if timing_available { percentile(&latencies, 0.5) } else { serde_json::Value::Null },
            "p95": if timing_available { percentile(&latencies, 0.95) } else { serde_json::Value::Null },
        },
        "tokens_per_decision": {
            "decisions_with_usage": prompt.len(),
            "mean_prompt": mean(&prompt),
            "mean_completion": mean(&completion),
        },
        "unavailable": {
            "larger_model_calls_on_eligible_cases": NOT_IN_SHADOW,
            "verified_completion_rate": NOT_IN_SHADOW,
            "regression_rate": NOT_IN_SHADOW,
        },
    })
}

/// Runs every case once, in fixture order, against `responder`. Sequential: no concurrency to vary.
pub async fn run_cases(fixture: &Fixture, responder: &dyn Responder) -> Vec<CaseResult> {
    let mut out = Vec::new();
    for (i, case) in fixture.cases.iter().enumerate() {
        let attempt = responder.ask(case, i).await;
        out.push(CaseResult {
            case: case.clone(),
            attempt,
        });
    }
    out
}

/// What kind of run produced a record. Only `completed` is evidence about a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunKind {
    /// A real model answered at least one case.
    Completed,
    /// No model was available, or none answered. No metrics.
    Blocked,
    /// A scripted responder, to test the harness. Not evidence about any model.
    ScriptedSelfTest,
    /// Recorded replies scored again.
    Replay,
}

impl RunKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Blocked => "blocked",
            Self::ScriptedSelfTest => "scripted_self_test",
            Self::Replay => "replay",
        }
    }
}

pub struct RunContext<'a> {
    pub kind: RunKind,
    pub blocked_reason: Option<String>,
    pub model: Option<crate::micro::ShadowIdentity>,
    pub timeout: Duration,
    pub repository: serde_json::Value,
    pub scripted: Option<&'static str>,
    pub fixture: &'a Fixture,
    /// The text of `heldout.freeze.json`, when available.
    pub freeze: Option<&'a str>,
    /// For a replay: the record whose replies were scored again. A replay is never a new inference run.
    pub replayed_from: Option<serde_json::Value>,
    /// The candidate being evaluated and what its operator declared about it (artifact, quantization, runtime,
    /// context limit, hardware); absent fields are reported as unreported, never guessed.
    pub candidate: Option<serde_json::Value>,
}

fn prediction(r: &CaseResult) -> serde_json::Value {
    let (status, nomination, rejection, provider_error) = match &r.attempt.nomination {
        Nomination::Valid(v) => ("valid", Some(crate::micro::response_json(v)), None, None),
        Nomination::Rejected(x) => (
            "rejected",
            None,
            Some(serde_json::json!({"code": x.code.as_str(), "detail": x.detail})),
            None,
        ),
        Nomination::ProviderFailed(e) => ("provider_failed", None, None, Some(e.clone())),
        Nomination::TimedOut => ("timed_out", None, None, None),
    };
    let class_correct = valid(r).map(|v| {
        r.case
            .label
            .classes
            .contains(&v.classification.unwrap_or(FailureClass::Unknown))
    });
    let strategy_false_positive = valid(r).and_then(|v| {
        (v.strategy.is_some() && v.applicability != Some(Applicability::NotApplicable)).then(|| {
            r.case.label.must_abstain || !r.case.label.strategies.contains(&v.strategy.unwrap())
        })
    });
    serde_json::json!({
        "case": r.case.id,
        "split": r.case.split.name(),
        "category": r.case.category,
        "failure_class": primary_class(&r.case),
        "execution_status": r.case.execution_status,
        "expected": {
            "classes": r.case.label.classes.iter().map(|c| c.as_str()).collect::<Vec<_>>(),
            "strategies": r.case.label.strategies.iter().map(|c| c.as_str()).collect::<Vec<_>>(),
            "must_abstain": r.case.label.must_abstain,
            "human_review": r.case.label.human_review,
        },
        "status": status,
        "nomination": nomination,
        "rejection": rejection,
        "provider_error": provider_error,
        "raw_reply": r.attempt.reply,
        "class_correct": class_correct,
        "strategy_false_positive": strategy_false_positive,
        "latency_ms": r.attempt.latency.as_millis() as u64,
        "prompt_tokens": r.attempt.prompt_tokens,
        "completion_tokens": r.attempt.completion_tokens,
    })
}

/// The run record: what ran, against what, with what settings, and how it scored. A blocked run
/// reports no metrics at all.
///
/// The primary evidence is `metrics.heldout_executed`: held-out cases whose failure was reproduced by running
/// real tools. Unexecuted synthetic cases are scored separately and are harness fixtures, not ground truth.
pub fn record(ctx: &RunContext<'_>, results: &[CaseResult]) -> serde_json::Value {
    let mut checks = fixture_checks(ctx.fixture);
    checks.push(frozen_check(ctx.fixture, ctx.freeze));
    let select = |split: Option<Split>, executed: Option<bool>| -> Vec<&CaseResult> {
        results
            .iter()
            .filter(|r| split.is_none_or(|s| r.case.split == s))
            .filter(|r| executed.is_none_or(|e| r.case.executed() == e))
            .collect()
    };
    let by_class = |split: Split| -> serde_json::Value {
        let mut classes: BTreeMap<&str, Vec<&CaseResult>> = BTreeMap::new();
        for r in results
            .iter()
            .filter(|r| r.case.split == split && r.case.executed())
        {
            classes.entry(primary_class(&r.case)).or_default().push(r);
        }
        classes
            .into_iter()
            .map(|(k, v)| (k.to_string(), metrics(&v)))
            .collect()
    };
    let has_metrics = ctx.kind != RunKind::Blocked;
    let executed = ctx.fixture.cases.iter().filter(|c| c.executed()).count();
    let frozen_intact = checks.last().is_some_and(|c| c.passed);
    serde_json::json!({
        "schema": "chip.micro-eval.v2",
        "status": ctx.kind.name(),
        "new_inference": matches!(ctx.kind, RunKind::Completed),
        "evidence_about_a_model": ctx.kind == RunKind::Completed && frozen_intact,
        "heldout_frozen_intact": frozen_intact,
        "blocked_reason": ctx.blocked_reason,
        "scripted_responder": ctx.scripted,
        "replayed_from": ctx.replayed_from,
        "candidate": ctx.candidate,
        "authority_granted": "none",
        "fixture": {
            "version": ctx.fixture.version,
            "sha256": ctx.fixture.sha256,
            "heldout_sha256": ctx.fixture.heldout_sha256,
            "cases": ctx.fixture.cases.len(),
            "executed_cases": executed,
            "unexecuted_cases": ctx.fixture.cases.len() - executed,
            "label_verification": {
                "executed_reproduced_and_verified": executed,
                "unexecuted_synthetic": ctx.fixture.cases.len() - executed,
                "independently_human_reviewed": ctx.fixture.cases.iter().filter(|c| c.label.human_review == "reviewed").count(),
                "human_review_pending": ctx.fixture.cases.iter().filter(|c| c.label.human_review == "pending").count(),
            },
            "checks": checks.iter().map(|c| serde_json::json!({"check": c.name, "passed": c.passed, "detail": c.detail})).collect::<Vec<_>>(),
        },
        "prompt_sha256": crate::micro::system_prompt_digest(),
        "contract": {
            "schema": crate::micro::SCHEMA,
            "schema_sha256": crate::micro::schema_digest(),
            "version": crate::micro::INTERIM_CONTRACT_VERSION,
            "version_basis": "interim: no Work Contract exists yet",
        },
        "model": ctx.model.as_ref().map(|m| serde_json::json!({"provider": m.provider, "model": m.model, "endpoint": m.endpoint})),
        "runtime_settings": {
            "max_output_tokens": crate::micro::MAX_OUTPUT_TOKENS,
            "temperature": 0.0,
            "json_object_output": true,
            "timeout_ms": ctx.timeout.as_millis() as u64,
            "concurrency": 1,
            "retries": 0,
        },
        "repository": ctx.repository,
        "metrics": if has_metrics {
            serde_json::json!({
                "heldout_executed": metrics(&select(Some(Split::Heldout), Some(true))),
                "heldout_unexecuted": metrics(&select(Some(Split::Heldout), Some(false))),
                "calibration_executed": metrics(&select(Some(Split::Calibration), Some(true))),
                "calibration_unexecuted": metrics(&select(Some(Split::Calibration), Some(false))),
                "heldout_all": metrics(&select(Some(Split::Heldout), None)),
                "calibration_all": metrics(&select(Some(Split::Calibration), None)),
                "by_failure_class": {
                    "heldout_executed": by_class(Split::Heldout),
                    "calibration_executed": by_class(Split::Calibration),
                },
            })
        } else {
            serde_json::Value::Null
        },
        "replies": results.iter().filter_map(|r| r.attempt.reply.as_ref().map(|reply| (r.case.id.clone(), reply.clone()))).collect::<BTreeMap<_, _>>(),
        "predictions": if has_metrics {
            serde_json::json!(results.iter().map(prediction).collect::<Vec<_>>())
        } else {
            serde_json::Value::Null
        },
    })
}

/// A run with no model: the record says so and reports nothing about any model.
pub fn blocked(
    fixture: &Fixture,
    reason: String,
    repository: serde_json::Value,
    freeze: Option<&str>,
    candidate: Option<serde_json::Value>,
) -> serde_json::Value {
    record(
        &RunContext {
            kind: RunKind::Blocked,
            blocked_reason: Some(reason),
            model: None,
            timeout: crate::micro::DEFAULT_TIMEOUT,
            repository,
            scripted: None,
            fixture,
            freeze,
            replayed_from: None,
            candidate,
        },
        &[],
    )
}

pub fn rejection_code_names() -> Vec<&'static str> {
    use RejectCode::*;
    [
        TooLarge,
        Malformed,
        DuplicateField,
        UnknownField,
        MissingField,
        WrongType,
        SchemaMismatch,
        InvalidEnum,
        InconsistentFields,
        ContractVersionMismatch,
        SnapshotMismatch,
        UnknownStrategy,
        StrategyNotOffered,
        UnknownEvidence,
        StaleEvidence,
        ScopeNotInEvidence,
        ScopeTooLarge,
    ]
    .iter()
    .map(|c| c.as_str())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Fixture {
        load(include_str!("../tests/fixtures/micro/fixture.json"))
            .expect("the committed fixture loads")
    }

    fn run(responder: &dyn Responder) -> Vec<CaseResult> {
        let f = fixture();
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(run_cases(&f, responder))
    }

    fn split(rs: &[CaseResult], s: Split) -> Vec<&CaseResult> {
        rs.iter().filter(|r| r.case.split == s).collect()
    }

    #[test]
    fn the_committed_fixture_passes_its_own_structural_checks() {
        let f = fixture();
        for c in fixture_checks(&f) {
            assert!(c.passed, "{}: {}", c.name, c.detail);
        }
        assert!(f.cases.len() >= 20);
        assert!(f.sha256.starts_with("sha256:"));
    }

    #[test]
    fn labels_cannot_come_from_a_model_and_every_snapshot_is_bounded() {
        let f = fixture();
        for c in &f.cases {
            assert!(
                LABEL_SOURCES.contains(&c.label.label_source.as_str()),
                "{}",
                c.id
            );
            assert_eq!(c.label.human_review, "pending", "{}", c.id);
            assert!(
                c.snapshot.diagnostics.len() <= crate::micro::MAX_DIAGNOSTIC_BYTES + 100,
                "{}",
                c.id
            );
            assert!(
                crate::micro::user_prompt(&c.snapshot).len() <= crate::micro::MAX_PROMPT_BYTES,
                "{}",
                c.id
            );
        }
        // Scoring never writes a label: the fixture is the same before and after a run.
        let before = fixture().sha256;
        let _ = run(&Scripted(Script::Oracle));
        assert_eq!(before, fixture().sha256);
        // The source of the labels names no model.
        let raw = include_str!("../tests/fixtures/micro/fixture.json").to_lowercase();
        for forbidden in [
            "\"prediction\"",
            "\"model_output\"",
            "\"labelled_by\": \"model\"",
            "gpt",
            "claude",
            "llama",
            "qwen",
        ] {
            assert!(!raw.contains(forbidden), "{forbidden}");
        }
    }

    #[test]
    fn a_fixture_that_does_not_parse_is_refused_whole() {
        for bad in [
            "",
            "{}",
            r#"{"schema":"chip.micro.v1","fixture_version":"x","cases":[{"id":"a"}]}"#,
            r#"{"schema":"other","fixture_version":"x","cases":[]}"#,
        ] {
            assert!(load(bad).is_err(), "{bad}");
        }
        // An unknown enum value in a label is refused, not skipped.
        let raw = include_str!("../tests/fixtures/micro/fixture.json").replacen(
            "test_assertion_failure",
            "test_assertion",
            1,
        );
        assert!(load(&raw).is_err());
    }

    #[test]
    fn the_oracle_scores_perfectly_which_validates_the_scoring() {
        let rs = run(&Scripted(Script::Oracle));
        for s in [Split::Calibration, Split::Heldout] {
            let m = metrics(&split(&rs, s));
            assert_eq!(m["schema_valid_rate"], 1.0, "{m}");
            assert_eq!(
                m["classification"]["accuracy_over_valid_replies"], 1.0,
                "{m}"
            );
            assert_eq!(m["strategy"]["false_positives"], 0, "{m}");
            assert_eq!(m["abstention"]["appropriate_abstention_rate"], 1.0, "{m}");
            assert_eq!(m["abstention"]["inappropriate_abstention_rate"], 0.0, "{m}");
        }
    }

    #[test]
    fn always_abstaining_is_perfect_on_abstention_cases_and_costly_elsewhere() {
        let rs = run(&Scripted(Script::AbstainAll));
        let m = metrics(&split(&rs, Split::Heldout));
        assert_eq!(m["schema_valid_rate"], 1.0);
        assert_eq!(m["classification"]["classified_rate"], 0.0);
        assert_eq!(m["abstention"]["appropriate_abstention_rate"], 1.0);
        assert_eq!(m["abstention"]["inappropriate_abstention_rate"], 1.0, "{m}");
        assert_eq!(m["strategy"]["nominations"], 0);
        assert!(
            m["strategy"]["false_positive_rate"].is_null(),
            "no nominations is unavailable, not zero"
        );
    }

    #[test]
    fn adversarial_replies_are_rejected_counted_and_never_become_classifications() {
        let rs = run(&Scripted(Script::Adversarial));
        let all: Vec<&CaseResult> = rs.iter().collect();
        let m = metrics(&all);
        let codes = m["rejection_codes"].as_object().unwrap();
        for code in [
            "malformed",
            "snapshot_mismatch",
            "contract_version_mismatch",
            "unknown_field",
            "unknown_strategy",
        ] {
            assert!(codes.contains_key(code), "{code} missing from {codes:?}");
        }
        assert!(codes.contains_key("stale_evidence") || codes.contains_key("unknown_evidence"));
        assert!(m["provider_failures"].as_u64().unwrap() > 0);
        // A rejected reply is not a prediction: replies = valid + rejected, failures have none.
        assert_eq!(
            m["replies_received"].as_u64().unwrap(),
            m["valid_replies"].as_u64().unwrap() + m["rejected_replies"].as_u64().unwrap()
        );
        // Wrong-but-valid replies are counted as wrong, including false-positive strategies.
        assert!(
            m["strategy"]["false_positives"].as_u64().unwrap() > 0,
            "{m}"
        );
        assert!(
            m["classification"]["accuracy_over_valid_replies"]
                .as_f64()
                .unwrap()
                < 1.0
        );
    }

    #[test]
    fn unavailable_measurements_are_null_with_a_reason_never_zero() {
        // Nothing was asked: every rate is null.
        let m = metrics(&[]);
        for path in [
            &m["schema_valid_rate"],
            &m["classification"]["accuracy_over_valid_replies"],
            &m["strategy"]["false_positive_rate"],
            &m["abstention"]["appropriate_abstention_rate"],
            &m["latency_ms"]["p50"],
            &m["tokens_per_decision"]["mean_prompt"],
        ] {
            assert!(path.is_null(), "{m}");
        }
        for key in [
            "larger_model_calls_on_eligible_cases",
            "verified_completion_rate",
            "regression_rate",
        ] {
            assert!(
                m["unavailable"][key].as_str().unwrap().contains("ablation"),
                "{key}"
            );
        }
        // The scripted responder reports no timing or usage; that is unavailable, not zero.
        let rs = run(&Scripted(Script::Oracle));
        let m = metrics(&rs.iter().collect::<Vec<_>>());
        assert!(
            m["latency_ms"]["p50"].is_null() && m["tokens_per_decision"]["mean_prompt"].is_null(),
            "{m}"
        );
        assert_eq!(m["tokens_per_decision"]["decisions_with_usage"], 0);
    }

    #[test]
    fn a_blocked_run_reports_no_metrics_and_says_it_is_not_evidence() {
        let f = fixture();
        let r = blocked(
            &f,
            "no shadow model is configured".into(),
            serde_json::json!({"git_commit": null}),
            None,
            None,
        );
        assert_eq!(r["status"], "blocked");
        assert_eq!(r["evidence_about_a_model"], false);
        assert!(r["metrics"].is_null() && r["results"].is_null());
        assert_eq!(r["authority_granted"], "none");
        assert!(r["model"].is_null());
        assert_eq!(r["fixture"]["sha256"], f.sha256);
    }

    #[test]
    fn a_scripted_run_is_labelled_and_a_replay_reproduces_the_held_out_metrics() {
        let f = fixture();
        let rs = run(&Scripted(Script::Adversarial));
        let ctx = RunContext {
            kind: RunKind::ScriptedSelfTest,
            blocked_reason: None,
            model: None,
            timeout: Duration::from_secs(1),
            repository: serde_json::json!({}),
            scripted: Some("adversarial"),
            fixture: &f,
            freeze: Some(include_str!("../tests/fixtures/micro/heldout.freeze.json")),
            replayed_from: None,
            candidate: None,
        };
        let rec = record(&ctx, &rs);
        assert_eq!(rec["status"], "scripted_self_test");
        assert_eq!(rec["evidence_about_a_model"], false);
        // Replay the recorded replies and score again.
        let replies: BTreeMap<String, String> = rec["replies"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
            .collect();
        let again = run(&Replay(replies));
        let ctx2 = RunContext {
            kind: RunKind::Replay,
            scripted: None,
            ..ctx
        };
        let rec2 = record(&ctx2, &again);
        for key in [
            "heldout_executed",
            "heldout_all",
            "calibration_executed",
            "calibration_all",
            "by_failure_class",
        ] {
            assert_eq!(
                rec["metrics"][key], rec2["metrics"][key],
                "{key} reproduces exactly"
            );
        }
        // A replay is not a new inference run and is not evidence about a model by itself.
        assert_eq!(rec2["status"], "replay");
        assert_eq!(rec2["new_inference"], false);
        assert_eq!(rec2["evidence_about_a_model"], false);
        // Failures and rejections stay in the held-out numbers; they are not dropped.
        let h = &rec2["metrics"]["heldout_executed"];
        assert!(
            h["rejected_replies"].as_u64().unwrap() + h["provider_failures"].as_u64().unwrap() > 0,
            "{h}"
        );
        assert!(h["abstention"]["cases_requiring_abstention_answered"].is_number());
        // Every case has a recorded prediction with its raw reply and its outcome.
        let predictions = rec2["predictions"].as_array().unwrap();
        assert_eq!(predictions.len(), f.cases.len());
        assert!(
            predictions
                .iter()
                .all(|p| p.get("raw_reply").is_some() && p.get("status").is_some())
        );
    }

    #[test]
    fn executed_and_unexecuted_cases_are_scored_separately_and_the_primary_set_is_executed() {
        let f = fixture();
        let rs = run(&Scripted(Script::Oracle));
        let ctx = RunContext {
            kind: RunKind::ScriptedSelfTest,
            blocked_reason: None,
            model: None,
            timeout: Duration::from_secs(1),
            repository: serde_json::json!({}),
            scripted: Some("oracle"),
            fixture: &f,
            freeze: Some(include_str!("../tests/fixtures/micro/heldout.freeze.json")),
            replayed_from: None,
            candidate: None,
        };
        let rec = record(&ctx, &rs);
        let executed = f.cases.iter().filter(|c| c.executed()).count();
        assert_eq!(rec["fixture"]["executed_cases"], executed);
        assert_eq!(rec["fixture"]["unexecuted_cases"], f.cases.len() - executed);
        assert_eq!(
            rec["fixture"]["label_verification"]["independently_human_reviewed"],
            0
        );
        let m = &rec["metrics"];
        let n = |k: &str| m[k]["cases"].as_u64().unwrap();
        assert_eq!(
            n("heldout_executed") + n("heldout_unexecuted"),
            n("heldout_all")
        );
        assert_eq!(
            n("calibration_executed") + n("calibration_unexecuted"),
            n("calibration_all")
        );
        assert!(n("heldout_executed") >= 10 && n("calibration_executed") >= 10);
        // Per failure class, over executed cases only; the classes partition them.
        let by = m["by_failure_class"]["heldout_executed"]
            .as_object()
            .unwrap();
        let total: u64 = by.values().map(|v| v["cases"].as_u64().unwrap()).sum();
        assert_eq!(total, n("heldout_executed"));
        assert!(by.contains_key("compile_error") && by.contains_key("test_assertion_failure"));
        // The oracle is a ceiling for the arithmetic, never evidence.
        assert_eq!(rec["evidence_about_a_model"], false);
    }

    #[test]
    fn the_held_out_set_must_match_its_freeze_and_a_changed_case_is_detected() {
        let f = fixture();
        let freeze = include_str!("../tests/fixtures/micro/heldout.freeze.json");
        let ok = frozen_check(&f, Some(freeze));
        assert!(ok.passed, "{}", ok.detail);
        // Changing one held-out case changes the hash and fails the check.
        let raw = include_str!("../tests/fixtures/micro/fixture.json");
        let heldout_id = f
            .cases
            .iter()
            .find(|c| c.split == Split::Heldout)
            .unwrap()
            .id
            .clone();
        let mut v: serde_json::Value = serde_json::from_str(raw).unwrap();
        for c in v["cases"].as_array_mut().unwrap() {
            if c["id"] == heldout_id.as_str() {
                c["snapshot"]["diagnostics"] = "changed after the freeze".into();
            }
        }
        let tampered = load(&v.to_string()).unwrap();
        assert!(!frozen_check(&tampered, Some(freeze)).passed);
        // Moving a case between splits is detected too.
        let mut v: serde_json::Value = serde_json::from_str(raw).unwrap();
        for c in v["cases"].as_array_mut().unwrap() {
            if c["id"] == heldout_id.as_str() {
                c["split"] = "calibration".into();
            }
        }
        assert!(!frozen_check(&load(&v.to_string()).unwrap(), Some(freeze)).passed);
        assert!(
            !frozen_check(&f, None).passed,
            "no freeze file is not a pass"
        );
        // A run against a tampered fixture does not claim to be held-out evidence.
        let rs = Vec::new();
        let ctx = RunContext {
            kind: RunKind::Completed,
            blocked_reason: None,
            model: None,
            timeout: Duration::from_secs(1),
            repository: serde_json::json!({}),
            scripted: None,
            fixture: &tampered,
            freeze: Some(freeze),
            replayed_from: None,
            candidate: None,
        };
        let rec = record(&ctx, &rs);
        assert_eq!(rec["heldout_frozen_intact"], false);
        assert_eq!(rec["evidence_about_a_model"], false);
    }

    #[test]
    fn wilson_intervals_have_the_right_shape_and_no_interval_for_no_data() {
        let w = wilson(0, 0);
        assert!(w["low"].is_null() && w["n"] == 0);
        let all = wilson(10, 10);
        assert!((all["high"].as_f64().unwrap() - 1.0).abs() < 1e-9);
        assert!(
            all["low"].as_f64().unwrap() > 0.69 && all["low"].as_f64().unwrap() < 0.73,
            "{all}"
        );
        let half = wilson(5, 10);
        let (lo, hi) = (
            half["low"].as_f64().unwrap(),
            half["high"].as_f64().unwrap(),
        );
        assert!(
            lo < 0.5 && hi > 0.5 && (lo - 0.2366).abs() < 0.01 && (hi - 0.7634).abs() < 0.01,
            "{half}"
        );
        let none = wilson(0, 10);
        assert_eq!(none["low"], 0.0);
    }

    #[test]
    fn candidates_are_never_substituted() {
        // The candidate file lists accepted names; the bench refuses a run whose configured model is not one.
        let spec: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/micro/candidates.json")).unwrap();
        let keys: Vec<&str> = spec["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["key"].as_str().unwrap())
            .collect();
        assert!(
            keys.contains(&"qwen2.5-coder-1.5b-instruct")
                && keys.contains(&"llama-3.2-1b-instruct")
        );
        let all: Vec<&str> = spec["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|c| c["accepted_model_names"].as_array().unwrap())
            .map(|n| n.as_str().unwrap())
            .collect();
        let unique: std::collections::BTreeSet<&&str> = all.iter().collect();
        assert_eq!(
            unique.len(),
            all.len(),
            "an accepted name belongs to exactly one candidate"
        );
    }

    #[test]
    fn the_rejection_codes_are_a_closed_named_set() {
        assert_eq!(rejection_code_names().len(), 17);
    }
}
