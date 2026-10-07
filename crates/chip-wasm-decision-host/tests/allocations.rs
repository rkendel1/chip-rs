//! Counts heap allocations around the decision call on a live instance. One test only, so no
//! other thread allocates during a measurement.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use chip_core::{EvidenceState, ImpactState};
use chip_wasm_decision_host::WasmDecisionEngine;
use common::*;

struct Counting;
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

#[test]
fn a_decision_on_a_live_instance_allocates_nothing() {
    let mut engine = WasmDecisionEngine::new(wasm_module()).unwrap();
    let state = minimal(EvidenceState::KnownValid, ImpactState::Unchanged);
    let canonical = state.canonical_bytes();

    // Warm up: the host's reusable encode buffer is already sized; wasmi's stacks settle.
    for _ in 0..100 {
        engine.decide(&state).unwrap();
        engine.decide_bytes(&canonical).unwrap();
    }

    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for _ in 0..1_000 {
        engine.decide_bytes(&canonical).unwrap();
    }
    let bytes_path = ALLOCATIONS.load(Ordering::Relaxed) - before;

    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for _ in 0..1_000 {
        engine.decide(&state).unwrap();
    }
    let typed_path = ALLOCATIONS.load(Ordering::Relaxed) - before;

    println!("allocations over 1000 decisions: decide_bytes {bytes_path}, decide {typed_path}");
    assert_eq!(
        bytes_path, 0,
        "decide_bytes must not allocate after initialization"
    );
    assert_eq!(
        typed_path, 0,
        "decide must not allocate after initialization"
    );
}
