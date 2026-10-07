//! Counts heap allocations on the decision-state hot path. One test only, so no other test
//! thread can allocate while a measurement is running.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use chip_core::{
    CapabilityDecisionState, CapabilityId, EvidenceState, GraphStateToken, ImpactState, InputValue,
};

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

fn allocations<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    let value = f();
    (value, ALLOCATIONS.load(Ordering::Relaxed) - before)
}

#[test]
fn the_hot_path_allocates_nothing_once_the_state_exists() {
    let graph = GraphStateToken::from_digest([7; 32]);
    let id = CapabilityId::new("cap.a").unwrap();

    // Construction from existing values: moves them in, allocates nothing.
    let (state, construct) = allocations(|| {
        CapabilityDecisionState::new(id, graph, EvidenceState::KnownStale, ImpactState::Impacted)
    });
    assert_eq!(construct, 0, "new() must not allocate");

    // Hashing straight from the state: no buffer, no allocation.
    let (digest, hashing) = allocations(|| state.state_digest());
    assert_eq!(hashing, 0, "state_digest() must not allocate");
    assert_ne!(digest, [0; 32]);

    // Encoding into a reused buffer: no allocation once the buffer has capacity.
    let mut buffer = Vec::with_capacity(256);
    let (_, encoding) = allocations(|| {
        for _ in 0..100 {
            buffer.clear();
            state.write_canonical(&mut buffer);
        }
    });
    assert_eq!(
        encoding, 0,
        "write_canonical() into a reused buffer must not allocate"
    );

    // The same holds with typed inputs present.
    let with_inputs = state
        .clone()
        .with_input("n", InputValue::Integer(1))
        .with_input("t", InputValue::Text("x".into()));
    let (_, hashing_inputs) = allocations(|| with_inputs.state_digest());
    assert_eq!(hashing_inputs, 0);

    // For reference: the convenience forms do allocate, exactly where expected.
    let (_, bytes) = allocations(|| state.canonical_bytes());
    let (_, token) = allocations(|| state.state_token());
    println!("allocations: canonical_bytes {bytes}, state_token {token}");
    assert_eq!(bytes, 1);
    assert_eq!(token, 1, "the token string is the only allocation");
}
