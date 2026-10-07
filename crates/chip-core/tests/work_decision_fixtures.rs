//! PR29: saved model replies stay parseable (or stay rejected) without an API call.

use std::fs;
use std::path::Path;

use chip_core::{
    Capability, CapabilityAvailability, CapabilityDescriptor, CapabilityId, ModelDecisionBoundary,
    WorkDecision, WorkDecisionBoundary,
};
use fx_core::{ModelResponse, Usage};

#[test]
fn every_fixture_is_read_as_its_name_says() {
    let capabilities = vec![Capability {
        descriptor: CapabilityDescriptor::new(
            CapabilityId::new("compute.selftest").unwrap(),
            "selftest",
            "described",
        ),
        availability: CapabilityAvailability::Available,
    }];
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/work-decision");
    let mut seen = 0;
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if !name.ends_with(".reply.txt") {
            continue;
        }
        seen += 1;
        let text = fs::read_to_string(&path).unwrap();
        let result = ModelDecisionBoundary.interpret(
            &ModelResponse::new("fixture-1", text, Usage::new(0, 0)),
            &capabilities,
        );
        if name.starts_with("valid-") {
            match result {
                Ok(WorkDecision::RequestCapability(r)) => {
                    assert_eq!(r.capability_id.as_str(), "compute.selftest", "{name}");
                    // Chip, not the fixture, names the execution.
                    assert!(r.execution_id.0.as_str().starts_with("model-"), "{name}");
                }
                other => panic!("{name}: expected a capability request, got {other:?}"),
            }
        } else if name.starts_with("rejected-") {
            assert!(result.is_err(), "{name}: must be rejected, got {result:?}");
        } else {
            panic!("{name}: fixture names start with valid- or rejected-");
        }
    }
    assert!(seen >= 4);
}
