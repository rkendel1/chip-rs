//! Training and evaluation of the tiny local decision model. Development only: nothing here is
//! linked into the runtime, and the runtime artifact holds only the resulting parameters.
//!
//! Reproducible from the corpus, the feature schema, the [`Config`] and the seed: training is
//! full-batch gradient descent in `f64` with no randomness (the seed fixes only the data split),
//! so the same inputs produce the same artifact bytes.

use std::collections::BTreeSet;

use chip_local_decision::eval::{Evaluation, Labeled, evaluate};
use chip_local_decision::features::{BASE_FEATURES, FROZEN, Token, Vocabulary, extract};
use chip_local_decision::{Decision, LocalDecisionModel};
use chip_reasoning_corpus::{Verdict, corpus};
use sha2::{Digest, Sha256};

pub mod mlp;
pub mod pr25;
pub mod report;

/// Everything that determines the trained model besides the data.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Config {
    pub seed: u64,
    /// L2 strength on learned weights (not on the bias or the frozen priors).
    pub lambda: f64,
    /// Added to the largest margin any training case that should escalate reaches, so the
    /// threshold leaves headroom the training data did not need.
    pub kappa: f32,
    pub learning_rate: f64,
    pub iterations: usize,
    /// Fixed weight (as a penalty) on each unfamiliar fact, an unfamiliar capability and
    /// dropped facts: anything unseen can only push toward `Escalate`.
    pub unfamiliar_penalty: f64,
}

/// The configuration the committed artifact was trained with, chosen by leave-one-out on the
/// development split (see `select`).
pub const RECORDED_CONFIG: Config = Config {
    seed: 42,
    lambda: 0.001,
    kappa: 0.5,
    learning_rate: 0.8,
    iterations: 8000,
    unfamiliar_penalty: 4.0,
};

pub fn labeled_corpus() -> Vec<Labeled> {
    corpus()
        .iter()
        .map(|case| Labeled {
            id: case.id.to_string(),
            state: case.decision_state(),
            expected: match case.expected {
                Verdict::Continue => Decision::Continue,
                Verdict::Escalate => Decision::Escalate,
            },
        })
        .collect()
}

/// SplitMix64: a tiny, fully specified generator, so the split never depends on a library.
pub(crate) struct SplitMix64(pub(crate) u64);

impl SplitMix64 {
    pub(crate) fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Split {
    pub train: Vec<usize>,
    pub validation: Vec<usize>,
    pub held_out: Vec<usize>,
}

impl Split {
    /// Training plus validation: the data every design and fitting decision may use. The held-out
    /// cases are evaluated once, at the end.
    pub fn development(&self) -> Vec<usize> {
        let mut dev: Vec<usize> = self.train.iter().chain(&self.validation).copied().collect();
        dev.sort_unstable();
        dev
    }
}

/// A seeded, stratified split of roughly 60/20/20. Strata: cases the baseline already settles
/// as `Continue`, cases that need judgment to `Continue`, and cases that escalate.
pub fn split(cases: &[Labeled], seed: u64) -> Split {
    let mut strata: [Vec<usize>; 3] = Default::default();
    for (i, c) in cases.iter().enumerate() {
        let stratum = match (c.expected, c.id.starts_with("det-")) {
            (Decision::Continue, true) => 0,
            (Decision::Continue, false) => 1,
            (Decision::Escalate, _) => 2,
        };
        strata[stratum].push(i);
    }
    let mut rng = SplitMix64(seed);
    let mut out = Split {
        train: vec![],
        validation: vec![],
        held_out: vec![],
    };
    for mut members in strata {
        members.sort_by(|a, b| cases[*a].id.cmp(&cases[*b].id));
        for i in (1..members.len()).rev() {
            members.swap(i, (rng.next() % (i as u64 + 1)) as usize);
        }
        let n = members.len();
        let part = |fraction: f64| {
            if n >= 3 {
                ((n as f64 * fraction).round() as usize).max(1)
            } else {
                0
            }
        };
        let (n_test, n_val) = (part(0.2), part(0.2));
        out.held_out.extend(&members[..n_test]);
        out.validation.extend(&members[n_test..n_test + n_val]);
        out.train.extend(&members[n_test + n_val..]);
    }
    for v in [&mut out.train, &mut out.validation, &mut out.held_out] {
        v.sort_unstable();
    }
    out
}

pub(crate) fn vocabulary_of(cases: &[&Labeled]) -> Vocabulary {
    let capabilities: BTreeSet<String> = cases
        .iter()
        .map(|c| c.state.capability_id.as_str().to_string())
        .collect();
    let tokens: BTreeSet<Token> = cases
        .iter()
        .flat_map(|c| c.state.inputs.iter().map(|(n, v)| Token::of(n, v)))
        .collect();
    Vocabulary {
        capabilities: capabilities.into_iter().collect(),
        tokens: tokens.into_iter().collect(),
    }
}

fn sigmoid(s: f64) -> f64 {
    1.0 / (1.0 + (-s.clamp(-30.0, 30.0)).exp())
}

/// Fits a model to `cases`: vocabulary from these cases only, logistic regression by full-batch
/// gradient descent, then a threshold calibrated on the model's own margins.
pub fn fit(cases: &[&Labeled], cfg: &Config) -> LocalDecisionModel {
    let vocab = vocabulary_of(cases);
    let dimension = BASE_FEATURES + vocab.capabilities.len() + vocab.tokens.len();
    let capabilities = BASE_FEATURES + vocab.capabilities.len();

    // Dense design matrix, built from the runtime's own extractor.
    let rows: Vec<Vec<f64>> = cases
        .iter()
        .map(|c| {
            let f = extract(&vocab, &c.state);
            let mut x = vec![0.0; dimension];
            for (i, v) in f.base.iter().enumerate() {
                x[i] = f64::from(*v);
            }
            if let Some(cap) = f.capability {
                x[BASE_FEATURES + usize::from(cap)] = 1.0;
            }
            for &t in &f.tokens[..f.token_count] {
                x[capabilities + usize::from(t)] = 1.0;
            }
            x
        })
        .collect();
    let y: Vec<f64> = cases
        .iter()
        .map(|c| f64::from(c.expected == Decision::Continue))
        .collect();

    let mut w = vec![0.0f64; dimension];
    for i in FROZEN {
        w[i] = -cfg.unfamiliar_penalty;
    }
    let mut bias = 0.0f64;
    let n = cases.len().max(1) as f64;
    for _ in 0..cfg.iterations {
        let mut grad = vec![0.0f64; dimension];
        let mut grad_bias = 0.0;
        for (x, &target) in rows.iter().zip(&y) {
            let s: f64 = bias + x.iter().zip(&w).map(|(a, b)| a * b).sum::<f64>();
            let g = sigmoid(s) - target;
            grad_bias += g;
            for (gi, xi) in grad.iter_mut().zip(x) {
                *gi += g * xi;
            }
        }
        for j in 0..dimension {
            if !FROZEN.contains(&j) {
                w[j] -= cfg.learning_rate * (grad[j] / n + cfg.lambda * w[j]);
            }
        }
        bias -= cfg.learning_rate * grad_bias / n;
    }

    // Two logits that differ by the learned score: continue = +s/2, escalate = -s/2.
    let half = |v: f64| (v / 2.0) as f32;
    let weights = [
        w.iter().map(|v| half(*v)).collect::<Vec<f32>>(),
        w.iter().map(|v| -half(*v)).collect::<Vec<f32>>(),
    ];
    let bias = [half(bias), -half(bias)];
    let provisional = LocalDecisionModel::new(vocab.clone(), weights.clone(), bias, 0.0)
        .expect("trained parameters are finite");

    // Threshold: above every margin a case that should escalate reached, plus headroom.
    let highest_escalate = cases
        .iter()
        .filter(|c| c.expected == Decision::Escalate)
        .map(|c| provisional.infer(&c.state).margin)
        .fold(0.0f32, f32::max);
    LocalDecisionModel::new(vocab, weights, bias, highest_escalate + cfg.kappa)
        .expect("threshold is finite")
}

fn pick<'a>(cases: &'a [Labeled], indices: &[usize]) -> Vec<&'a Labeled> {
    indices.iter().map(|&i| &cases[i]).collect()
}

/// Leave-one-out: each case is judged by a model that never saw it (vocabulary included).
pub fn leave_one_out(cases: &[Labeled], indices: &[usize], cfg: &Config) -> Evaluation {
    Evaluation::merge(indices.iter().map(|&held| {
        let rest: Vec<usize> = indices.iter().copied().filter(|&i| i != held).collect();
        let model = fit(&pick(cases, &rest), cfg);
        evaluate(Some(&model), std::slice::from_ref(&cases[held]))
    }))
}

/// Chooses `lambda` and `kappa` by leave-one-out on the development data only: fewest false
/// continues first, then most true continues, then the least conservative (smallest lambda, then kappa), which sits at the edge of the safe region.
pub fn select(cases: &[Labeled], development: &[usize], base: &Config) -> (Config, Evaluation) {
    let mut best: Option<(Config, Evaluation)> = None;
    for lambda in [0.0001, 0.001, 0.01, 0.03] {
        for kappa in [0.0f32, 0.5, 1.0, 2.0] {
            let cfg = Config {
                lambda,
                kappa,
                ..*base
            };
            let result = leave_one_out(cases, development, &cfg);
            let key = |e: &Evaluation, c: &Config| {
                (
                    std::cmp::Reverse(e.guarded.fp + e.learned.fp),
                    std::cmp::Reverse(c.lambda.to_bits()),
                    std::cmp::Reverse(c.kappa.to_bits()),
                    c.kappa.to_bits(),
                )
            };
            let better = match &best {
                None => true,
                Some((bc, be)) => key(&result, &cfg) > key(be, bc),
            };
            if better {
                best = Some((cfg, result));
            }
        }
    }
    best.expect("the grid is not empty")
}

/// The model the repository ships: trained on the development split with `cfg`.
pub fn train_final(cases: &[Labeled], cfg: &Config) -> (LocalDecisionModel, Split) {
    let split = split(cases, cfg.seed);
    let model = fit(&pick(cases, &split.development()), cfg);
    (model, split)
}

/// SHA-256 of the corpus as the model sees it: id, expected decision and canonical state bytes.
pub fn corpus_digest(cases: &[Labeled]) -> String {
    let mut hasher = Sha256::new();
    for c in cases {
        hasher.update(c.id.as_bytes());
        hasher.update([0, u8::from(c.expected == Decision::Continue)]);
        hasher.update(c.state.canonical_bytes());
    }
    hex(&hasher.finalize())
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn artifact_sha256(model: &LocalDecisionModel) -> String {
    hex(&Sha256::digest(model.to_bytes()))
}
