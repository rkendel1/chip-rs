//! The session can reach nothing. These checks read the sources and the manifest.

use std::fs;
use std::path::Path;

fn read(relative: &str) -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)).unwrap()
}

#[test]
fn only_the_abi_file_opts_in_to_unsafe_code_and_it_has_none_of_its_own() {
    let abi = read("src/abi.rs");
    assert_eq!(abi.matches("allow(unsafe_code)").count(), 1);
    assert!(
        !abi.contains("unsafe {") && !abi.contains("unsafe fn") && !abi.contains("extern \""),
        "the exports come from the shared macro"
    );
    for file in ["src/lib.rs", "src/session.rs"] {
        let text = read(file);
        assert!(
            !text.contains("unsafe") || file == "src/lib.rs" && !text.contains("unsafe {"),
            "{file}"
        );
        assert!(!text.contains("allow(unsafe_code)"), "{file}");
    }
    assert!(read("src/lib.rs").contains("#[cfg(target_arch = \"wasm32\")]\nmod abi;"));
}

#[test]
fn the_session_names_no_ambient_authority_and_no_provider() {
    for file in ["src/lib.rs", "src/session.rs", "src/abi.rs"] {
        let code: String = read(file)
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for banned in [
            "std::fs",
            "std::net",
            "std::process",
            "std::env",
            "std::thread",
            "Instant",
            "SystemTime",
            "tokio",
            "reqwest",
            "chip_legacy_http",
            "fetch(",
            "getenv",
            "static mut",
            "thread_local",
            "OnceLock",
            "LazyLock",
        ] {
            assert!(!code.contains(banned), "{file} must not contain {banned}");
        }
    }
}

#[test]
fn dependencies_are_the_chip_and_fx_crates_the_session_exists_to_join() {
    let manifest = read("Cargo.toml");
    let dependencies = manifest
        .split("[dependencies]")
        .nth(1)
        .unwrap()
        .split("\n[")
        .next()
        .unwrap();
    for allowed in [
        "chip-core",
        "chip-inference",
        "chip-fx",
        "fx-core",
        "fx-wasm-abi",
        "async-trait",
        "serde_json",
    ] {
        assert!(dependencies.contains(allowed), "{allowed}");
    }
    for banned in ["chip-legacy-http", "reqwest", "tokio", "fx-single-attempt"] {
        assert!(
            !dependencies.contains(banned),
            "{banned} must not be a dependency"
        );
    }
}
