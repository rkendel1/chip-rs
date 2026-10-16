//! The recovery limitation is accepted for the first release only while callers can see it. This test
//! fails if the warning disappears from any place a caller reads, or from the release artifact.

use std::path::PathBuf;
use std::process::Command;

const MARKER: &str = "RETRYING A TURN MAY REPEAT EXTERNAL EFFECTS";

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn read(relative: &str) -> String {
    std::fs::read_to_string(root().join(relative)).unwrap_or_else(|e| panic!("{relative}: {e}"))
}

#[test]
fn the_warning_is_in_the_readme_the_release_notes_and_the_limitations_contract() {
    for file in [
        "README.md",
        "RELEASE_NOTES.md",
        "docs/product/release-limitations.md",
    ] {
        assert!(read(file).contains(MARKER), "{file} must carry: {MARKER}");
    }
}

#[test]
fn the_release_notes_ship_in_the_release_artifact() {
    assert!(
        read("scripts/package-release.sh").contains("RELEASE_NOTES.md"),
        "package-release.sh must copy RELEASE_NOTES.md into the artifact"
    );
}

#[test]
fn the_warning_is_in_the_text_a_caller_sees_from_the_binary() {
    let out = Command::new(env!("CARGO_BIN_EXE_chip"))
        .args(["work", "--not-a-flag"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(
        text.contains(MARKER),
        "chip work's usage must carry the warning: {text}"
    );
}

#[test]
fn every_unsupported_behavior_in_the_contract_names_what_happens_when_it_is_requested() {
    let doc = read("docs/product/release-limitations.md");
    for id in [
        "permission_modes",
        "provider_native_tool_loop",
        "silent_tool_continuation",
        "step_limit_pause",
        "images_and_vision",
        "subagents_mcp_skills_memory",
        "interactive_permission_prompts",
        "cancel_after_tool",
        "persistence_failure_after_work_ran",
        "duplicate_side_effects_guard",
    ] {
        let row = doc
            .lines()
            .find(|l| l.contains(&format!("(`{id}`)")))
            .unwrap_or_else(|| panic!("{id} has no row in release-limitations.md"));
        let cells: Vec<&str> = row.split('|').map(str::trim).collect();
        assert!(
            cells.len() >= 4 && cells[3].len() > 20,
            "{id}: no stated runtime behavior"
        );
    }
}
