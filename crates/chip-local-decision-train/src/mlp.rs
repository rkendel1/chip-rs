//! An experiment, not a candidate: would a tiny hidden layer find conjunctions of facts that a
//! linear model cannot? It is evaluated with the same leave-one-out protocol and the same
//! pessimistic priors, and reported next to the linear model. Nothing here ships: if it does not
//! clearly beat the linear model without a false continue, it is rejected, and the runtime never
//! learns it exists.

use chip_local_decision::Decision;
use chip_local_decision::eval::Labeled;
use chip_local_decision::features::{BASE_FEATURES, FROZEN, extract};

use crate::{SplitMix64, vocabulary_of};

#[derive(Debug, Clone, Copy)]
pub struct MlpConfig {
    pub hidden: usize,
    pub lambda: f64,
    pub kappa: f64,
    pub learning_rate: f64,
    pub iterations: usize,
    pub seed: u64,
    pub unfamiliar_penalty: f64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MlpResult {
    /// Learned `Continue` where `Continue` was expected.
    pub true_continues: usize,
    /// Learned `Continue` where `Escalate` was expected: the false continues.
    pub false_continues: usize,
    /// True continues the deterministic baseline escalates and the impact guard permits.
    pub additional_safe: usize,
}

fn dense(
    vocab: &chip_local_decision::Vocabulary,
    state: &chip_core::CapabilityDecisionState,
) -> Vec<f64> {
    let f = extract(vocab, state);
    let dimension = BASE_FEATURES + vocab.capabilities.len() + vocab.tokens.len();
    let mut x = vec![0.0; dimension];
    for (i, v) in f.base.iter().enumerate() {
        x[i] = f64::from(*v);
    }
    if let Some(c) = f.capability {
        x[BASE_FEATURES + usize::from(c)] = 1.0;
    }
    for &t in &f.tokens[..f.token_count] {
        x[BASE_FEATURES + vocab.capabilities.len() + usize::from(t)] = 1.0;
    }
    x
}

struct Net {
    w1: Vec<Vec<f64>>,
    b1: Vec<f64>,
    v: Vec<f64>,
    c: f64,
    penalty: f64,
}

impl Net {
    fn score(&self, x: &[f64]) -> (f64, Vec<f64>) {
        let h: Vec<f64> = self
            .w1
            .iter()
            .zip(&self.b1)
            .map(|(row, b)| (b + row.iter().zip(x).map(|(w, xi)| w * xi).sum::<f64>()).tanh())
            .collect();
        let frozen: f64 = FROZEN.iter().map(|&i| -self.penalty * x[i]).sum();
        (
            self.c + self.v.iter().zip(&h).map(|(a, b)| a * b).sum::<f64>() + frozen,
            h,
        )
    }
}

/// Fits on `train`, calibrates the threshold on the training margins, and judges `test`.
pub fn fit_and_judge(train: &[&Labeled], test: &Labeled, cfg: &MlpConfig) -> (Decision, f64) {
    let vocab = vocabulary_of(train);
    let rows: Vec<Vec<f64>> = train.iter().map(|c| dense(&vocab, &c.state)).collect();
    let y: Vec<f64> = train
        .iter()
        .map(|c| f64::from(c.expected == Decision::Continue))
        .collect();
    let dimension = rows[0].len();
    let mut rng = SplitMix64(cfg.seed);
    let mut uniform = move || (rng.next() >> 11) as f64 / (1u64 << 53) as f64 - 0.5;
    let mut net = Net {
        w1: (0..cfg.hidden)
            .map(|_| (0..dimension).map(|_| uniform() * 0.6).collect())
            .collect(),
        b1: vec![0.0; cfg.hidden],
        v: (0..cfg.hidden).map(|_| uniform()).collect(),
        c: 0.0,
        penalty: cfg.unfamiliar_penalty,
    };
    let n = train.len() as f64;
    for _ in 0..cfg.iterations {
        let mut g_w1 = vec![vec![0.0; dimension]; cfg.hidden];
        let mut g_b1 = vec![0.0; cfg.hidden];
        let mut g_v = vec![0.0; cfg.hidden];
        let mut g_c = 0.0;
        for (x, &target) in rows.iter().zip(&y) {
            let (s, h) = net.score(x);
            let ds = 1.0 / (1.0 + (-s.clamp(-30.0, 30.0)).exp()) - target;
            g_c += ds;
            for j in 0..cfg.hidden {
                g_v[j] += ds * h[j];
                let dpre = ds * net.v[j] * (1.0 - h[j] * h[j]);
                g_b1[j] += dpre;
                for (g, xi) in g_w1[j].iter_mut().zip(x) {
                    *g += dpre * xi;
                }
            }
        }
        for j in 0..cfg.hidden {
            net.v[j] -= cfg.learning_rate * (g_v[j] / n + cfg.lambda * net.v[j]);
            net.b1[j] -= cfg.learning_rate * g_b1[j] / n;
            for k in 0..dimension {
                net.w1[j][k] -= cfg.learning_rate * (g_w1[j][k] / n + cfg.lambda * net.w1[j][k]);
            }
        }
        net.c -= cfg.learning_rate * g_c / n;
    }
    let highest_escalate = train
        .iter()
        .zip(&rows)
        .filter(|(c, _)| c.expected == Decision::Escalate)
        .map(|(_, x)| net.score(x).0)
        .fold(0.0f64, f64::max);
    let margin = net.score(&dense(&vocab, &test.state)).0;
    let decision = if margin.is_finite() && margin > highest_escalate + cfg.kappa {
        Decision::Continue
    } else {
        Decision::Escalate
    };
    (decision, margin)
}

/// Leave-one-out over `indices`.
pub fn leave_one_out(cases: &[Labeled], indices: &[usize], cfg: &MlpConfig) -> MlpResult {
    let mut result = MlpResult::default();
    for &held in indices {
        let rest: Vec<&Labeled> = indices
            .iter()
            .filter(|&&i| i != held)
            .map(|&i| &cases[i])
            .collect();
        let case = &cases[held];
        let (decision, _) = fit_and_judge(&rest, case, cfg);
        if decision == Decision::Continue {
            match case.expected {
                Decision::Continue => {
                    result.true_continues += 1;
                    let baseline = chip_local_decision::deterministic_decision(&case.state);
                    if baseline == Decision::Escalate
                        && case.state.impact == chip_core::ImpactState::Unchanged
                    {
                        result.additional_safe += 1;
                    }
                }
                Decision::Escalate => result.false_continues += 1,
            }
        }
    }
    result
}

/// A small grid over the hidden layer, regularization, headroom and seed; returns every result
/// so nothing is cherry-picked.
pub fn experiment(cases: &[Labeled], indices: &[usize]) -> Vec<(MlpConfig, MlpResult)> {
    let mut out = Vec::new();
    for hidden in [4usize] {
        for lambda in [0.001, 0.01] {
            for kappa in [0.0, 1.0, 2.0] {
                for seed in [42u64, 43] {
                    let cfg = MlpConfig {
                        hidden,
                        lambda,
                        kappa,
                        learning_rate: 0.3,
                        iterations: 2000,
                        seed,
                        unfamiliar_penalty: 4.0,
                    };
                    out.push((cfg, leave_one_out(cases, indices, &cfg)));
                }
            }
        }
    }
    out
}
