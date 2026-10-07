//! PR31: the standard operations are real, described capabilities, opt-in, and deterministic.

use chip_compute::{
    ComputeExecutor, HASH_EXPECTED_SHA256, HASH_INPUT, HASH_INTENT, SELFTEST_INTENT,
    SYSTEM_INFO_INTENT,
};
use chip_core::CapabilityProvider;
use sha2::{Digest, Sha256};

#[test]
fn the_expected_digest_is_the_sha256_of_the_fixed_text() {
    // Independent of Compute and of any model.
    let digest: String = Sha256::digest(HASH_INPUT.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(digest, HASH_EXPECTED_SHA256);
    // The text is embedded in the operation's source, so it must not need escaping.
    assert!(HASH_INPUT.is_ascii() && !HASH_INPUT.contains(['"', '\\', '\n']));
}

#[tokio::test]
async fn the_default_set_is_unchanged_and_the_standard_set_is_opt_in() {
    let default = ComputeExecutor::new().capabilities().await.unwrap();
    assert_eq!(default.len(), 1);
    assert_eq!(default[0].id.as_str(), SELFTEST_INTENT);

    let standard = ComputeExecutor::new()
        .with_standard_operations()
        .capabilities()
        .await
        .unwrap();
    let ids: Vec<&str> = standard.iter().map(|d| d.id.as_str()).collect();
    assert_eq!(ids, [HASH_INTENT, SELFTEST_INTENT, SYSTEM_INFO_INTENT]);
}

#[tokio::test]
async fn descriptions_say_what_each_capability_does_and_hide_how() {
    let standard = ComputeExecutor::new()
        .with_standard_operations()
        .capabilities()
        .await
        .unwrap();
    for d in &standard {
        assert!(!d.description.is_empty(), "{} has no description", d.id);
        assert!(d.inputs.is_empty(), "{} declares inputs", d.id);
        let shown = format!("{d:?}");
        for detail in ["hashlib", "platform.", "print(", "python", ".py"] {
            assert!(!shown.contains(detail), "{} leaks {detail}", d.id);
        }
    }
    let hash = standard
        .iter()
        .find(|d| d.id.as_str() == HASH_INTENT)
        .unwrap();
    assert!(hash.description.contains(HASH_INPUT));
    assert!(hash.description.to_lowercase().contains("sha-256"));
}

// ---- PR32: the opaque set -------------------------------------------------------------------

use chip_compute::{
    OP_A_DESCRIPTION, OP_A_INTENT, OP_B_DESCRIPTION, OP_B_INTENT, OP_C_DESCRIPTION, OP_C_INTENT,
};

#[tokio::test]
async fn the_opaque_set_replaces_the_operations_and_its_ids_reveal_nothing() {
    let opaque = ComputeExecutor::new()
        .with_opaque_operations()
        .capabilities()
        .await
        .unwrap();
    let ids: Vec<&str> = opaque.iter().map(|d| d.id.as_str()).collect();
    // Exactly three: the built-in compute.selftest is gone, because its id would give op_c away.
    assert_eq!(ids, [OP_A_INTENT, OP_B_INTENT, OP_C_INTENT]);
    for d in &opaque {
        let shown = format!("{} {}", d.id, d.name).to_lowercase();
        for word in [
            "hash", "sha", "digest", "self", "test", "info", "system", "runtime", "version",
        ] {
            assert!(!shown.contains(word), "{shown} reveals '{word}'");
        }
    }
}

#[tokio::test]
async fn opaque_descriptions_carry_the_meaning_and_hide_the_mechanism() {
    let opaque = ComputeExecutor::new()
        .with_opaque_operations()
        .capabilities()
        .await
        .unwrap();
    let by = |id: &str| opaque.iter().find(|d| d.id.as_str() == id).unwrap();
    assert_eq!(by(OP_A_INTENT).description, OP_A_DESCRIPTION);
    assert_eq!(by(OP_B_INTENT).description, OP_B_DESCRIPTION);
    assert_eq!(by(OP_C_INTENT).description, OP_C_DESCRIPTION);
    assert!(OP_A_DESCRIPTION.contains("SHA-256"));
    for d in &opaque {
        assert!(d.inputs.is_empty(), "{} takes no arguments", d.id);
        let shown = format!("{d:?}");
        // No mechanism, and not the answer: neither the source nor the input nor the digest.
        for detail in [
            "hashlib",
            "platform.",
            "print(",
            "python",
            ".py",
            HASH_INPUT,
            HASH_EXPECTED_SHA256,
        ] {
            assert!(!shown.contains(detail), "{} leaks {detail}", d.id);
        }
    }
}

// ---- PR33: the same operations dealt to the opaque ids in every possible way ------------------

use chip_compute::{HASH_DESCRIPTION, SELFTEST_DESCRIPTION, SYSTEM_INFO_DESCRIPTION};

const IDS: [&str; 3] = [OP_A_INTENT, OP_B_INTENT, OP_C_INTENT];
const PERMUTATIONS: [[usize; 3]; 6] = [
    [0, 1, 2],
    [0, 2, 1],
    [1, 0, 2],
    [1, 2, 0],
    [2, 0, 1],
    [2, 1, 0],
];

#[tokio::test]
async fn every_assignment_keeps_each_description_with_its_operation() {
    for [h, s, t] in PERMUTATIONS {
        let found = ComputeExecutor::new()
            .with_opaque_assignment(IDS[h], IDS[s], IDS[t])
            .capabilities()
            .await
            .unwrap();
        let ids: Vec<&str> = found.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(
            ids, IDS,
            "the set is the same three ids, whatever the assignment"
        );
        let text = |id: &str| {
            found
                .iter()
                .find(|d| d.id.as_str() == id)
                .unwrap()
                .description
                .clone()
        };
        assert_eq!(text(IDS[h]), HASH_DESCRIPTION);
        assert_eq!(text(IDS[s]), SYSTEM_INFO_DESCRIPTION);
        assert_eq!(text(IDS[t]), SELFTEST_DESCRIPTION);
    }
}

#[tokio::test]
async fn the_default_opaque_set_is_the_first_assignment() {
    let a = ComputeExecutor::new()
        .with_opaque_operations()
        .capabilities()
        .await
        .unwrap();
    let b = ComputeExecutor::new()
        .with_opaque_assignment(OP_A_INTENT, OP_B_INTENT, OP_C_INTENT)
        .capabilities()
        .await
        .unwrap();
    assert_eq!(a, b);
}
