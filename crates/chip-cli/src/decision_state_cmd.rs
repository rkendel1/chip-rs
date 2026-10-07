//! `chip-cli decision-state` and `chip-cli --benchmark-decision-state`.
//!
//! Both operate on values passed in. Neither reads the repository, initializes the graph,
//! executes anything or calls a model.

use std::hint::black_box;
use std::time::Instant;

use chip_core::{
    CapabilityDecisionState, CapabilityId, EvidenceState, GraphStateToken, ImpactState, InputValue,
};

const USAGE: &str = "usage: chip-cli decision-state --capability ID --graph-state sha256:HEX \
--evidence valid|stale|unknown --impact impacted|unchanged";

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// Returns the process exit code. A state that cannot be validated renders nothing.
pub fn decision_state(args: &[String]) -> i32 {
    let (Some(capability), Some(graph), Some(evidence), Some(impact)) = (
        flag(args, "--capability"),
        flag(args, "--graph-state"),
        flag(args, "--evidence"),
        flag(args, "--impact"),
    ) else {
        eprintln!("{USAGE}");
        return 2;
    };
    let built = (|| -> Result<CapabilityDecisionState, String> {
        Ok(CapabilityDecisionState::new(
            CapabilityId::new(capability).map_err(|e| format!("{e:?}"))?,
            GraphStateToken::parse(&graph).map_err(|e| e.to_string())?,
            EvidenceState::from_wire_name(&evidence).map_err(|e| e.to_string())?,
            ImpactState::from_wire_name(&impact).map_err(|e| e.to_string())?,
        ))
    })();
    let state = match built {
        Ok(state) => state,
        Err(message) => {
            eprintln!("{message}");
            return 1;
        }
    };
    println!("Capability:\n  {}\n", state.capability_id);
    println!("GraphState:\n  {}\n", state.graph_state);
    println!("Evidence:\n  {}\n", state.evidence_state.wire_name());
    println!("Impact:\n  {}\n", state.impact.wire_name());
    println!("StateToken:\n  {}", state.state_token().as_str());
    0
}

struct Stage {
    name: &'static str,
    median: f64,
    p95: f64,
    max: f64,
}

/// Times `op` in `samples` batches of `batch` calls; reports ns per call over the batches.
fn measure(name: &'static str, samples: usize, batch: usize, mut op: impl FnMut()) -> Stage {
    for _ in 0..batch.min(1_000) {
        op();
    }
    let mut per_op: Vec<f64> = (0..samples)
        .map(|_| {
            let started = Instant::now();
            for _ in 0..batch {
                op();
            }
            started.elapsed().as_nanos() as f64 / batch as f64
        })
        .collect();
    per_op.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let at =
        |q: f64| per_op[((per_op.len() as f64 * q).ceil() as usize).clamp(1, per_op.len()) - 1];
    Stage {
        name,
        median: at(0.5),
        p95: at(0.95),
        max: *per_op.last().unwrap(),
    }
}

pub fn benchmark(args: &[String]) -> i32 {
    let iterations: usize = args.first().and_then(|n| n.parse().ok()).unwrap_or(200_000);
    let samples = 100usize;
    let batch = (iterations / samples).max(1);

    let id = CapabilityId::new("cap.a").unwrap();
    let graph = GraphStateToken::from_digest([7; 32]);
    let state = CapabilityDecisionState::new(
        id.clone(),
        graph,
        EvidenceState::KnownStale,
        ImpactState::Impacted,
    );
    let with_inputs = state
        .clone()
        .with_input("attempt", InputValue::Integer(1))
        .with_input("dry_run", InputValue::Bool(true));
    let mut buffer = Vec::with_capacity(256);
    let mut wire = 0u8;

    let stages = [
        measure(
            "construct (clones the id: 1 allocation)",
            samples,
            batch,
            || {
                black_box(CapabilityDecisionState::new(
                    black_box(id.clone()),
                    graph,
                    EvidenceState::KnownStale,
                    ImpactState::Impacted,
                ));
            },
        ),
        measure("canonical bytes (allocates a Vec)", samples, batch, || {
            black_box(black_box(&state).canonical_bytes());
        }),
        measure("canonical bytes (reused buffer)", samples, batch, || {
            buffer.clear();
            black_box(&state).write_canonical(&mut buffer);
            wire = wire.wrapping_add(buffer.len() as u8);
        }),
        measure("state digest (hash from state)", samples, batch, || {
            black_box(black_box(&state).state_digest());
        }),
        measure(
            "state token (digest + token string)",
            samples,
            batch,
            || {
                black_box(black_box(&state).state_token());
            },
        ),
        measure("state digest, 2 typed inputs", samples, batch, || {
            black_box(black_box(&with_inputs).state_digest());
        }),
        measure("construct + digest (hot path)", samples, batch, || {
            let s = CapabilityDecisionState::new(
                black_box(id.clone()),
                graph,
                EvidenceState::KnownStale,
                ImpactState::Impacted,
            );
            black_box(s.state_digest());
        }),
    ];
    black_box(wire);

    println!("Decision state benchmark");
    println!("========================");
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
        "Iterations per stage: {} ({samples} samples x {batch})",
        samples * batch
    );
    println!("Canonical state: {} bytes", state.canonical_bytes().len());
    println!("Wire JSON: {} bytes", state.to_wire_json().len());
    println!();
    println!(
        "{:<42} {:>10} {:>10} {:>10}",
        "Stage (ns per operation)", "median", "p95", "max"
    );
    for s in &stages {
        println!(
            "{:<42} {:>10.1} {:>10.1} {:>10.1}",
            s.name, s.median, s.p95, s.max
        );
    }
    println!();
    println!("Allocation counts are asserted by the chip-core test decision_state_alloc:");
    println!("  new() 0, state_digest() 0, write_canonical() into a reused buffer 0,");
    println!("  canonical_bytes() 1, state_token() 1.");
    println!("This is a baseline on this machine, not a performance claim.");
    0
}
