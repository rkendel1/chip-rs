//! The recovery packet: the canonical, deterministic statement of what a recovered session
//! knows, hashed so that candidates can be compared byte for byte.
//!
//! It contains only information the *session* holds (objective, status, plan, failed approaches,
//! verified repairs, unresolved failures with the SHA-256 of the pinned diagnostic payload, open
//! hypotheses, escalations, checkpoint id, pending decision, observation count). Nothing in it
//! comes from the storage engine, so a candidate whose packet differs lost or invented something.

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::backend::Backend;
use crate::recovery::RecoveredSession;
use crate::schema::SessionStatus;
use crate::store::SessionMemory;

pub fn sha256(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Canonical packet for a recovered session. `m` is used only to read the pinned diagnostics'
/// payloads, whose hash proves they survived.
pub fn packet<B: Backend>(m: &SessionMemory<B>, rec: &RecoveredSession) -> Value {
    let mut unresolved: Vec<(String, Vec<String>, String, String)> = rec
        .unresolved_failures
        .iter()
        .map(|t| {
            let d = t.diagnostic.clone().unwrap_or_default();
            let s = m
                .payload(&d)
                .ok()
                .flatten()
                .as_deref()
                .map(sha256)
                .unwrap_or_default();
            (t.command.clone(), t.failed_tests.clone(), d, s)
        })
        .collect();
    unresolved.sort();
    let mut failed: Vec<(String, String)> = rec
        .failed_approaches
        .iter()
        .map(|a| (a.id.clone(), a.action.clone()))
        .collect();
    failed.sort();
    let mut verified: Vec<(String, String)> = rec
        .verified_repairs
        .iter()
        .map(|r| (r.id.clone(), r.verified_by.clone().unwrap_or_default()))
        .collect();
    verified.sort();
    let mut open: Vec<String> = rec.open_hypotheses.iter().map(|h| h.id.clone()).collect();
    open.sort();
    let mut esc: Vec<(String, String, Vec<String>)> = rec
        .escalations
        .iter()
        .map(|e| (e.id.clone(), e.reason.clone(), e.outstanding.clone()))
        .collect();
    esc.sort();
    let status = if rec.session.status == SessionStatus::Escalated {
        "escalated"
    } else {
        "active"
    };
    json!({
        "objective": rec.session.objective, "status": status,
        "plan": rec.plan.as_ref().map(|p| p.steps.clone()),
        "plan_revisions": rec.plan.as_ref().map_or(0, |p| p.revision),
        "failed_approaches": failed, "verified_repairs": verified,
        "unresolved_failures": unresolved, "open_hypotheses": open, "escalations": esc,
        "checkpoint": rec.checkpoint.as_ref().map(|c| c.id.clone()),
        "pending": rec.session.pending.as_ref().map(|p| p.description.clone()),
        "observations": rec.observations,
    })
}

/// `(canonical JSON text, SHA-256 of that text)`.
pub fn packet_and_hash<B: Backend>(
    m: &SessionMemory<B>,
    rec: &RecoveredSession,
) -> (String, String) {
    let text = packet(m, rec).to_string();
    let hash = sha256(&text);
    (text, hash)
}
