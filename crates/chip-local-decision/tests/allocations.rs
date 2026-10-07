//! Counts heap allocations on the inference hot path. One test only, so no other thread
//! allocates during a measurement.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use chip_core::{EvidenceState, ImpactState, InputValue};
use chip_local_decision::{LocalDecider, LocalDecisionModel, PolicyMode, extract};
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
fn feature_extraction_inference_and_the_decider_allocate_nothing() {
    let model = LocalDecisionModel::embedded().unwrap();
    let decider = LocalDecider::new(Some(model.clone()), PolicyMode::LearnedGuarded);
    let s = state(
        "tests.run",
        EvidenceState::KnownStale,
        ImpactState::Unchanged,
    )
    .with_input("prerequisites_met", InputValue::Bool(true))
    .with_input("change_affects_capability", InputValue::Bool(false))
    .with_input("last_outcome", InputValue::Text("success".into()))
    .with_input("never_trained_on_this", InputValue::Integer(7));

    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for _ in 0..1_000 {
        std::hint::black_box(extract(model.vocabulary(), &s));
    }
    let extraction = ALLOCATIONS.load(Ordering::Relaxed) - before;

    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for _ in 0..1_000 {
        std::hint::black_box(model.infer(&s));
    }
    let inference = ALLOCATIONS.load(Ordering::Relaxed) - before;

    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for _ in 0..1_000 {
        std::hint::black_box(decider.decide(&s));
    }
    let decision = ALLOCATIONS.load(Ordering::Relaxed) - before;

    println!(
        "allocations over 1000 calls: extract {extraction}, infer {inference}, decide {decision}"
    );
    assert_eq!((extraction, inference, decision), (0, 0, 0));
}
