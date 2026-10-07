//! The manifest (provenance, not part of any decision) and the human-readable report.

use chip_local_decision::eval::{Evaluation, Labeled, evaluate};
use chip_local_decision::{Decision, FEATURE_SCHEMA, LocalDecisionModel, MODEL_SCHEMA};

use crate::{Config, Split, artifact_sha256, corpus_digest, fit, leave_one_out};

fn ids(cases: &[Labeled], indices: &[usize]) -> String {
    let list: Vec<String> = indices
        .iter()
        .map(|&i| format!("\"{}\"", cases[i].id))
        .collect();
    format!("[{}]", list.join(", "))
}

/// Descriptive provenance for the artifact. Never an input to a decision.
pub fn manifest_json(
    cases: &[Labeled],
    split: &Split,
    cfg: &Config,
    model: &LocalDecisionModel,
    dev_loo: &Evaluation,
) -> String {
    format!(
        "{{\n  \"schema\": \"{MODEL_SCHEMA}\",\n  \"feature_schema\": \"{FEATURE_SCHEMA}\",\n  \"model\": \"tiny-linear\",\n  \"seed\": {seed},\n  \"config\": {{\"lambda\": {lambda}, \"kappa\": {kappa}, \"learning_rate\": {lr}, \"iterations\": {iters}, \"unfamiliar_penalty\": {penalty}}},\n  \"corpus\": {{\"cases\": {n}, \"digest_sha256\": \"{digest}\"}},\n  \"split\": {{\n    \"train\": {train},\n    \"validation\": {validation},\n    \"held_out\": {held_out}\n  }},\n  \"fitted_on\": \"train + validation\",\n  \"selection\": {{\"method\": \"leave-one-out on train + validation\", \"loo_false_continues\": {fp}, \"loo_true_continues\": {tp}}},\n  \"artifact\": {{\"bytes\": {bytes}, \"sha256\": \"{sha}\", \"parameters\": {params}, \"dimension\": {dim}, \"capabilities\": {caps}, \"fact_tokens\": {tokens}}}\n}}\n",
        seed = cfg.seed,
        lambda = cfg.lambda,
        kappa = cfg.kappa,
        lr = cfg.learning_rate,
        iters = cfg.iterations,
        penalty = cfg.unfamiliar_penalty,
        n = cases.len(),
        digest = corpus_digest(cases),
        train = ids(cases, &split.train),
        validation = ids(cases, &split.validation),
        held_out = ids(cases, &split.held_out),
        fp = dev_loo.guarded.fp.max(dev_loo.learned.fp),
        tp = dev_loo.guarded.tp,
        bytes = model.to_bytes().len(),
        sha = artifact_sha256(model),
        params = model.parameter_count(),
        dim = model.dimension(),
        caps = model.vocabulary().capabilities.len(),
        tokens = model.vocabulary().tokens.len(),
    )
}

fn subset(cases: &[Labeled], indices: &[usize]) -> Vec<Labeled> {
    indices.iter().map(|&i| cases[i].clone()).collect()
}

fn per_case(e: &Evaluation, wanted: &[&str]) -> String {
    let name = |d: Decision| {
        if d == Decision::Continue {
            "Continue"
        } else {
            "Escalate"
        }
    };
    let mut out = String::new();
    for id in wanted {
        if let Some(c) = e.cases.iter().find(|c| c.id == *id) {
            out.push_str(&format!(
                "  {:<7} expected {:<8} deterministic {:<8} learned {:<8} guarded {}\n",
                c.id,
                name(c.expected),
                name(c.deterministic),
                c.learned_raw.map_or("-", name),
                name(c.guarded),
            ));
        }
    }
    out
}

/// The full evaluation: held-out (judged once), leave-one-out, and the in-sample numbers
/// clearly labelled as such.
pub fn full_report(
    cases: &[Labeled],
    split: &Split,
    cfg: &Config,
    model: &LocalDecisionModel,
) -> String {
    let dev = split.development();
    let held_out = subset(cases, &split.held_out);
    let held = evaluate(Some(model), &held_out);
    let dev_loo = leave_one_out(cases, &dev, cfg);
    let all_loo = leave_one_out(cases, &(0..cases.len()).collect::<Vec<_>>(), cfg);
    let in_sample = evaluate(Some(model), cases);
    let hard = ["lj-01", "lj-04", "lj-06", "lj-09"];

    let mut out = String::new();
    out.push_str(&format!(
        "Model: tiny-linear, two logits over {FEATURE_SCHEMA}\nParameters: {} ({} dimensions x 2 logits + 2 biases + threshold); vocabulary {} capabilities, {} fact tokens\nArtifact size: {} bytes (sha256 {})\nResident size: {} bytes (parameters plus vocabulary)\nThreshold on margin: {:.4}\nSeed {} / lambda {} / kappa {} / {} iterations\n\n",
        model.parameter_count(), model.dimension(), model.vocabulary().capabilities.len(), model.vocabulary().tokens.len(),
        model.to_bytes().len(), artifact_sha256(model), model.resident_bytes(), model.threshold(),
        cfg.seed, cfg.lambda, cfg.kappa, cfg.iterations,
    ));
    out.push_str(&format!(
        "Training cases: {}\nValidation cases: {}\nHeld-out cases: {}\n(32 cases is very small: every number below has wide uncertainty, and the feature design was\ninformed by reading all 32 cases, so even the held-out split tests the fitted weights, not the design.)\n\n",
        split.train.len(), split.validation.len(), split.held_out.len(),
    ));
    out.push_str("== HELD-OUT (never used for fitting or selection) ==\n");
    out.push_str(&held.render("Held-out"));
    out.push_str("\n== LEAVE-ONE-OUT on train + validation (each case judged by a model that never saw it) ==\n");
    out.push_str(&dev_loo.render("Leave-one-out (dev)"));
    out.push_str("\n== LEAVE-ONE-OUT on all 32 cases (same configuration; informational) ==\n");
    out.push_str(&all_loo.render("Leave-one-out (all)"));
    out.push_str("\nHard cases, leave-one-out (judged out-of-sample):\n");
    out.push_str(&per_case(&all_loo, &hard));
    out.push_str("\n== HIDDEN-LAYER EXPERIMENT (rejected unless it beats the linear model; leave-one-out on train + validation) ==\n");
    out.push_str(&mlp_summary(cases, &dev));
    out.push_str(
        "\n== IN-SAMPLE on all 32 cases (25 of them were fitted; NOT evidence of quality) ==\n",
    );
    out.push_str(&in_sample.render("In-sample"));
    out.push_str("\nHard cases, in-sample:\n");
    out.push_str(&per_case(&in_sample, &hard));
    out
}

/// Fits a fresh model with `cfg` and returns it with its split (used by tests).
pub fn refit(cases: &[Labeled], cfg: &Config) -> (LocalDecisionModel, Split) {
    let split = crate::split(cases, cfg.seed);
    let dev: Vec<&Labeled> = split.development().iter().map(|&i| &cases[i]).collect();
    (fit(&dev, cfg), split)
}

fn mlp_summary(cases: &[Labeled], dev: &[usize]) -> String {
    let results = crate::mlp::experiment(cases, dev);
    let safe: Vec<_> = results
        .iter()
        .filter(|(_, r)| r.false_continues == 0)
        .collect();
    let best_safe = safe
        .iter()
        .map(|(_, r)| r.additional_safe)
        .max()
        .unwrap_or(0);
    let most_unsafe = results
        .iter()
        .map(|(_, r)| r.false_continues)
        .max()
        .unwrap_or(0);
    format!(
        "tiny tanh network (4 hidden units), {} configurations (regularization x headroom x seed):\n  configurations with zero false continues: {} of {}\n  most additional safe local decisions among those: {}\n  worst false continues across the grid: {}\n  verdict: {}\n",
        results.len(),
        safe.len(),
        results.len(),
        best_safe,
        most_unsafe,
        if best_safe == 0 {
            "no gain over the linear model, and less safe at low regularization; rejected"
        } else {
            "gain found; compare against the linear model before adopting"
        },
    )
}
