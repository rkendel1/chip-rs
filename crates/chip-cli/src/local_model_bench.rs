//! `chip-cli --benchmark-local-model` and `--evaluate-local-model`.
//!
//! Native only: the question here is model quality and footprint, not deployment. Neither
//! command reads a repository, calls a model provider or executes anything; the model is the
//! artifact embedded in the binary.

use std::hint::black_box;
use std::time::Instant;

use chip_core::CapabilityDecisionState;
use chip_local_decision::eval::{Labeled, evaluate};
use chip_local_decision::{
    Decision, LocalDecider, LocalDecisionModel, PolicyMode, deterministic_decision, extract,
};
use chip_reasoning_corpus::{Verdict, corpus};

#[derive(Clone, Copy)]
struct Stats {
    median: f64,
    p95: f64,
    p99: f64,
    max: f64,
}

fn stats(mut values: Vec<f64>) -> Stats {
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let at =
        |q: f64| values[((values.len() as f64 * q).ceil() as usize).clamp(1, values.len()) - 1];
    Stats {
        median: at(0.5),
        p95: at(0.95),
        p99: at(0.99),
        max: *values.last().unwrap(),
    }
}

/// Per-call nanoseconds over `samples` batches of `batch` calls.
fn per_call(samples: usize, batch: usize, mut op: impl FnMut()) -> Stats {
    for _ in 0..batch.min(2_000) {
        op();
    }
    stats(
        (0..samples)
            .map(|_| {
                let started = Instant::now();
                for _ in 0..batch {
                    op();
                }
                started.elapsed().as_nanos() as f64 / batch as f64
            })
            .collect(),
    )
}

fn each_call(count: usize, mut op: impl FnMut()) -> Stats {
    op();
    stats(
        (0..count)
            .map(|_| {
                let started = Instant::now();
                op();
                started.elapsed().as_nanos() as f64
            })
            .collect(),
    )
}

fn fmt(ns: f64) -> String {
    if ns >= 1_000.0 {
        format!("{:.2} us", ns / 1_000.0)
    } else {
        format!("{ns:.1} ns")
    }
}

fn row(name: &str, s: Stats) {
    println!(
        "  {:<42} {:>10} {:>10} {:>10} {:>10}",
        name,
        fmt(s.median),
        fmt(s.p95),
        fmt(s.p99),
        fmt(s.max)
    );
}

fn labeled() -> Vec<Labeled> {
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

pub fn benchmark(args: &[String]) -> i32 {
    let iterations: usize = args.first().and_then(|n| n.parse().ok()).unwrap_or(200_000);
    let samples = 100.min(iterations);
    let batch = (iterations / samples).max(1);
    let embedded_len = LocalDecisionModel::embedded()
        .map(|m| m.to_bytes().len())
        .unwrap_or(0);
    let model = match LocalDecisionModel::embedded() {
        Ok(model) => model,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let artifact = model.to_bytes();
    let embedded_len = artifact.len();
    let states: Vec<CapabilityDecisionState> = labeled().into_iter().map(|c| c.state).collect();
    let decider = LocalDecider::new(Some(model.clone()), PolicyMode::LearnedGuarded);

    println!("Local decision model benchmark (native)");
    println!("=======================================");
    println!();
    println!(
        "Build: {}",
        if cfg!(debug_assertions) {
            "debug (use --release for a meaningful baseline)"
        } else {
            "release"
        }
    );
    println!(
        "Artifact: {embedded_len} bytes; {} parameters; {} dimensions",
        model.parameter_count(),
        model.dimension()
    );
    println!(
        "Resident model: {} bytes (parameters plus vocabulary; computed)",
        model.resident_bytes()
    );
    println!(
        "Corpus states cycled: {}; iterations per stage: {}",
        states.len(),
        samples * batch
    );
    println!();
    println!(
        "  {:<42} {:>10} {:>10} {:>10} {:>10}",
        "", "median", "p95", "p99", "max"
    );

    println!("Initialization");
    row(
        "model load (parse + checksum + validate)",
        each_call((iterations / 100).max(50), || {
            black_box(LocalDecisionModel::from_bytes(black_box(&artifact)).unwrap());
        }),
    );
    row(
        "decider setup",
        each_call((iterations / 100).max(50), || {
            black_box(LocalDecider::new(
                Some(model.clone()),
                PolicyMode::LearnedGuarded,
            ));
        }),
    );

    let mut i = 0usize;
    let mut next = || {
        i = (i + 1) % states.len();
        &states[i]
    };
    let features: Vec<_> = states
        .iter()
        .map(|s| extract(model.vocabulary(), s))
        .collect();
    let learned_raw: Vec<Decision> = states.iter().map(|s| model.infer(s).decision).collect();

    println!("Warm inference");
    let mut j = 0usize;
    let extraction = per_call(samples, batch, || {
        j = (j + 1) % states.len();
        black_box(extract(model.vocabulary(), black_box(&states[j])));
    });
    row("feature extraction", extraction);
    let mut k = 0usize;
    let inference = per_call(samples, batch, || {
        k = (k + 1) % features.len();
        black_box(model.infer_features(black_box(&features[k])).unwrap());
    });
    row("model inference (logits + threshold)", inference);
    let mut m = 0usize;
    let mapping = per_call(samples, batch, || {
        m = (m + 1) % states.len();
        // Decision mapping: the baseline plus the guard, from an already-computed learned answer.
        let s = &states[m];
        let det = deterministic_decision(black_box(s));
        let guarded = match (det, learned_raw[m], s.impact) {
            (Decision::Continue, _, _) => Decision::Continue,
            (_, Decision::Continue, chip_core::ImpactState::Unchanged) => Decision::Continue,
            _ => Decision::Escalate,
        };
        black_box(guarded);
    });
    row("decision mapping (baseline + guard)", mapping);

    println!("End to end (state -> features -> model -> typed decision)");
    let learned = per_call(samples, batch, || {
        black_box(model.infer(black_box(next())));
    });
    row("learned: model.infer", learned);
    let guarded = per_call(samples, batch, || {
        black_box(decider.decide(black_box(next())));
    });
    row("learned: LocalDecider (all policies)", guarded);
    let deterministic = per_call(samples, batch, || {
        black_box(deterministic_decision(black_box(next())));
    });
    row("deterministic native", deterministic);

    println!();
    println!(
        "Learned / deterministic (median): {:.1}x",
        learned.median / deterministic.median.max(0.001)
    );
    println!();
    println!(
        "Allocations are asserted by chip-local-decision tests/allocations.rs: extraction, inference"
    );
    println!(
        "and the decider allocate nothing. This is a baseline on this machine, not a performance claim."
    );
    0
}

pub fn evaluate_corpus(_args: &[String]) -> i32 {
    let model = match LocalDecisionModel::embedded() {
        Ok(model) => model,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let cases = labeled();
    let evaluation = evaluate(Some(&model), &cases);
    println!("Local decision model on the reasoning corpus");
    println!("============================================");
    println!();
    println!(
        "NOTE: 25 of these 32 cases were used to fit the model, so these numbers are in-sample and"
    );
    println!("show nothing about quality. Held-out and leave-one-out results come from");
    println!("`cargo run -p chip-local-decision-train -- report`.");
    println!();
    print!("{}", evaluation.render("Corpus"));
    if evaluation.guarded.fp > 0 || evaluation.learned.fp > 0 {
        println!("\nFAILED: a false continue was produced.");
        return 1;
    }
    0
}
