#![cfg(feature = "runtime")]
//! Live test against the real Laya model. Skipped unless Laya is installed and READY
//! (`ml-runtime model install laya`); installation needs the network, inference does not.
//! Run: cargo test -p chip-local-ml --features runtime --test live_laya -- --nocapture

use chip_core::TestLocalReasoner;
use chip_local_ml::{RustMLReasoner, installation_status};
use chip_reasoning_corpus::{corpus, evaluate};
use rust_ml_runtime::{InstalledModelStatus, Runtime};

#[test]
fn laya_replays_the_corpus_offline() {
    let runtime = Runtime::builder().allow_remote_fallback(false).build();
    match installation_status(&runtime, "laya") {
        Ok(InstalledModelStatus::Ready) => {}
        other => {
            eprintln!("SKIPPED — Laya is not installed and ready ({other:?})");
            return;
        }
    }
    // Inference must work with the network unusable: point every proxy at a closed port.
    for variable in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        // SAFETY: single-threaded test setup before any other thread reads the environment.
        unsafe { std::env::set_var(variable, "http://127.0.0.1:9") };
    }
    let reasoner = RustMLReasoner::load_installed(&runtime, "laya").expect("READY model loads");
    eprintln!("model: {}", reasoner.provenance());
    let cases = corpus();
    let baseline = evaluate(&TestLocalReasoner::default(), &cases);
    let result = evaluate(&reasoner, &cases);
    assert_eq!(result.total, 32);
    assert!(
        result.cases.iter().all(|c| c.error.is_none()),
        "every case produced a verdict"
    );
    eprintln!(
        "PASSED — real Laya: {}/{} correct, {} false continues, safe improvement {:?}",
        result.correct,
        result.total,
        result.false_continues(),
        result.safe_improvement_over(&baseline)
    );
}
