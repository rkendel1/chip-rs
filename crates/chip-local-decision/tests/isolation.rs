use std::fs;
use std::path::Path;

#[test]
fn the_runtime_crate_has_only_the_two_allowed_dependencies_and_no_forbidden_surface() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest = fs::read_to_string(root.join("Cargo.toml")).unwrap();
    let runtime = manifest.split("[dev-dependencies]").next().unwrap();
    let deps: Vec<&str> = runtime
        .split("[dependencies]")
        .nth(1)
        .unwrap()
        .lines()
        .filter_map(|l| l.split_once('=').map(|(n, _)| n.trim()))
        .collect();
    assert_eq!(
        deps,
        ["chip-core", "chip-wasm-decision"],
        "runtime dependencies"
    );

    for entry in fs::read_dir(root.join("src")).unwrap() {
        let path = entry.unwrap().path();
        let source = fs::read_to_string(&path).unwrap();
        let code: String = source
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
            .to_lowercase();
        for banned in [
            "std::fs",
            "std::net",
            "std::process",
            "std::thread",
            "std::env",
            "std::time",
            "command::new",
            "tcpstream",
            "reqwest",
            "tokio",
            "hyper",
            "serde",
            "json",
            "chip_graph",
            "chip-graph",
            "fx_core",
            "fx-core",
            "fx_provider",
            "chip_compute",
            "appport",
            "laya",
            "candle",
            "onnx",
            "wasmi",
            "tokenizer",
            "tokenize",
            "prompt",
            "chip_wasm_decision_host",
            "async",
        ] {
            assert!(
                !code.contains(banned),
                "{} must not mention {banned}",
                path.display()
            );
        }
    }
}

#[test]
fn training_code_is_not_part_of_the_runtime() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for entry in fs::read_dir(root.join("src")).unwrap() {
        let path = entry.unwrap().path();
        let code = fs::read_to_string(&path).unwrap().to_lowercase();
        for training in [
            "gradient",
            "backprop",
            "learning_rate",
            "sigmoid",
            "epoch",
            "fn fit",
        ] {
            assert!(
                !code.contains(training),
                "{} looks like training code ({training})",
                path.display()
            );
        }
    }
}
