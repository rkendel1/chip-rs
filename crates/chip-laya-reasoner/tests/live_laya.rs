#![cfg(feature = "laya")]
//! Live test against a real Laya checkpoint (for example the `typed-decisions` subfolder of
//! `convaiinnovations/laya`). Skipped unless CHIP_LAYA_MODEL_DIR points at a local checkpoint:
//! this test never downloads anything.
//!   CHIP_LAYA_MODEL_DIR=/path/to/laya CHIP_LAYA_SUBFOLDER=typed-decisions \
//!     cargo test -p chip-laya-reasoner --features laya --test live_laya -- --nocapture

use chip_core::TestLocalReasoner;
use chip_laya_reasoner::LayaReasoner;
use chip_reasoning_corpus::{corpus, evaluate};

#[test]
fn real_laya_replays_the_corpus() {
    let Ok(dir) = std::env::var("CHIP_LAYA_MODEL_DIR") else {
        eprintln!("SKIPPED — model not installed (set CHIP_LAYA_MODEL_DIR to a local checkpoint)");
        return;
    };
    let reasoner = match std::env::var("CHIP_LAYA_SUBFOLDER") {
        Ok(sub) if !sub.is_empty() => LayaReasoner::from_dir_subfolder(&dir, &sub),
        _ => LayaReasoner::from_dir(&dir),
    }
    .expect("the configured checkpoint loads");
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
        "PASSED — real Laya: {}/{} correct, {} false continues, {} needless escalations, safe improvement {:?}",
        result.correct,
        result.total,
        result.false_continues(),
        result.needless_escalations(),
        result.safe_improvement_over(&baseline)
    );
}
