//! `chip-cli --benchmark-wasm-decision`: what the tiny Wasm decision module costs.
//!
//! Module initialization, decision invocation on a live instance, and fresh-instance
//! end-to-end are reported separately, next to the native reference, so the number that matters
//! is the Wasm / native ratio rather than an absolute latency. Nothing here reads a repository,
//! calls a model or executes anything.

use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::Instant;

use chip_core::{
    CapabilityDecisionState, CapabilityId, EvidenceState, GraphStateToken, ImpactState,
};
use chip_wasm_decision_host::{WasmDecisionModule, decide_native};

const BUILD_HINT: &str =
    "cargo build -p chip-wasm-decision --target wasm32-unknown-unknown --profile wasm-decision";

#[derive(Clone, Copy)]
struct Stats {
    median: f64,
    p95: f64,
    max: f64,
}

fn stats(mut values: Vec<f64>) -> Stats {
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let at =
        |q: f64| values[((values.len() as f64 * q).ceil() as usize).clamp(1, values.len()) - 1];
    Stats {
        median: at(0.5),
        p95: at(0.95),
        max: *values.last().unwrap(),
    }
}

/// Per-call nanoseconds, measured in batches so timer overhead does not dominate.
fn per_call(iterations: usize, mut op: impl FnMut()) -> Stats {
    let samples = 100.min(iterations);
    let batch = (iterations / samples).max(1);
    for _ in 0..batch.min(1_000) {
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

/// Per-call nanoseconds, each call timed on its own (for operations that cost microseconds).
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

fn micros(ns: f64) -> String {
    if ns >= 1_000_000.0 {
        format!("{:.2} ms", ns / 1_000_000.0)
    } else if ns >= 1_000.0 {
        format!("{:.2} us", ns / 1_000.0)
    } else {
        format!("{ns:.1} ns")
    }
}

fn row(name: &str, s: Stats) {
    println!(
        "  {:<44} {:>12} {:>12} {:>12}",
        name,
        micros(s.median),
        micros(s.p95),
        micros(s.max)
    );
}

fn default_artifact() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/wasm32-unknown-unknown/wasm-decision/chip_wasm_decision.wasm")
}

/// Returns the process exit code. 3 means the optional Wasm artifact is not built (skipped).
pub fn benchmark(args: &[String]) -> i32 {
    let mut path = None;
    let mut counts = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--wasm" {
            path = iter.next().map(PathBuf::from);
        } else if let Ok(n) = arg.parse::<usize>() {
            counts.push(n.max(10));
        }
    }
    if counts.is_empty() {
        counts = vec![1_000, 10_000, 100_000];
    }
    let path = path.unwrap_or_else(default_artifact);
    let wasm = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(_) => {
            println!(
                "SKIPPED: the Wasm decision module is not built ({}).",
                path.display()
            );
            println!("Build it with:\n  {BUILD_HINT}");
            return 3;
        }
    };

    let module = match WasmDecisionModule::compile(&wasm) {
        Ok(module) => module,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let mut engine = match module.instantiate() {
        Ok(engine) => engine,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };

    let state = CapabilityDecisionState::new(
        CapabilityId::new("cap.a").unwrap(),
        GraphStateToken::from_digest([7; 32]),
        EvidenceState::KnownValid,
        ImpactState::Unchanged,
    );
    let canonical = state.canonical_bytes();
    // The result must agree with the native reference before any timing means anything.
    if engine.decide(&state) != Ok(decide_native(&state)) {
        eprintln!("the Wasm module disagrees with the native reference");
        return 1;
    }

    println!("Wasm decision benchmark");
    println!("=======================");
    println!();
    println!(
        "Build: {}",
        if cfg!(debug_assertions) {
            "debug (use --release for a meaningful baseline)"
        } else {
            "release"
        }
    );
    let sibling_raw = path
        .parent()
        .and_then(Path::parent)
        .map(|dir| dir.join("release/chip_wasm_decision.wasm"))
        .and_then(|p| std::fs::metadata(p).ok());
    println!("Decision module: {}", path.display());
    println!(
        "  optimized bytes (opt-level z, LTO, panic=abort, stripped): {}",
        wasm.len()
    );
    match sibling_raw {
        Some(meta) => println!("  raw bytes (plain --release build): {}", meta.len()),
        None => println!(
            "  raw bytes: not built (cargo build -p chip-wasm-decision --target wasm32-unknown-unknown --release)"
        ),
    }
    println!("Canonical state: {} bytes", canonical.len());
    println!("Decision module memory: one persistent instance is reused for all decision calls.");

    for iterations in counts {
        let init_runs = (iterations / 100).max(5);
        println!();
        println!("Iterations: {iterations} decisions; {init_runs} initializations");
        println!("  {:<44} {:>12} {:>12} {:>12}", "", "median", "p95", "max");

        println!("Module initialization (once per process)");
        row(
            "compile + validate module",
            each_call(init_runs, || {
                black_box(WasmDecisionModule::compile(black_box(&wasm)).unwrap());
            }),
        );
        row(
            "instantiate + ABI check (compiled module)",
            each_call(init_runs, || {
                black_box(module.instantiate().unwrap());
            }),
        );

        println!("Decision invocation (live instance)");
        let wasm_bytes = per_call(iterations, || {
            black_box(engine.decide_bytes(black_box(&canonical)).unwrap());
        });
        let wasm_typed = per_call(iterations, || {
            black_box(engine.decide(black_box(&state)).unwrap());
        });
        let native = per_call(iterations, || {
            black_box(decide_native(black_box(&state)));
        });
        let native_bytes = per_call(iterations, || {
            black_box(chip_wasm_decision::decide_bytes(black_box(&canonical)).unwrap());
        });
        row("wasm: decide_bytes (canonical bytes in)", wasm_bytes);
        row("wasm: decide (encode state + call)", wasm_typed);
        row("native: typed reference", native);
        row("native: same byte parser as the module", native_bytes);

        println!("End to end (fresh instance per decision: what NOT to do)");
        row(
            "instantiate + decide",
            each_call(init_runs, || {
                let mut fresh = module.instantiate().unwrap();
                black_box(fresh.decide_bytes(black_box(&canonical)).unwrap());
            }),
        );

        println!("Wasm / native overhead (median)");
        println!(
            "  decide_bytes vs typed native: {:.1}x",
            wasm_bytes.median / native.median.max(0.001)
        );
        println!(
            "  decide_bytes vs native bytes: {:.1}x",
            wasm_bytes.median / native_bytes.median.max(0.001)
        );
    }
    println!();
    println!(
        "Allocations are asserted by chip-wasm-decision-host tests/allocations.rs: a live-instance"
    );
    println!(
        "decision allocates nothing after initialization; the module itself has no allocator."
    );
    println!("This is a baseline on this machine, not a performance claim.");
    0
}
