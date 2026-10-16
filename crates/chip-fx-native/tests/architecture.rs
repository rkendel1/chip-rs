//! Credentials are acquired in Chip's host layer, in one place, and nowhere else.

use std::fs;
use std::path::PathBuf;

fn sources() -> Vec<(String, String)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| {
            (
                e.file_name().to_string_lossy().into_owned(),
                fs::read_to_string(e.path()).unwrap(),
            )
        })
        .collect()
}

#[test]
fn only_the_credential_module_reads_the_environment() {
    for (name, text) in sources() {
        let reads_env = text.contains("std::env") || text.contains("env::var");
        assert_eq!(
            reads_env,
            name == "credentials.rs",
            "{name}: only credentials.rs may read the process environment"
        );
    }
}

#[test]
fn no_source_reads_files_or_a_vault() {
    for (name, text) in sources() {
        for forbidden in ["std::fs", "credentials.enc", ".llm", "keyring"] {
            assert!(!text.contains(forbidden), "{name} must not use {forbidden}");
        }
    }
}

#[test]
fn credentials_are_resolved_into_settings_not_into_the_fx_request() {
    // The FX request type is built by chip-fx from a ModelRequest, which has no credential field.
    let lib =
        fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs")).unwrap();
    assert!(!lib.contains("CredentialSource") || lib.contains("pub use credentials::"));
}
