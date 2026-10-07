//! Black-box acceptance test for `chip-cli slice`.
//!
//! The contract under test: a StateToken is only emitted from
//!
//!   validated capability catalog + validated architecture snapshot + non-empty capability slice
//!
//! and `slice` never initializes, mutates, executes, calls a model or reaches any external
//! capability provider. It consumes graph state; it does not create it.
//!
//! The token is the stable artifact. The snapshot id printed beside it is provenance and is
//! deliberately never hard-coded here. The token represents the *declared structural* graph
//! slice; whether evidence also needs a content token is a separate decision, so content-only
//! edits are intentionally not tested.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const CATALOG: &str = r#"{
  "schema": "chip.capabilities.v1",
  "capabilities": [
    { "id": "cap.a", "graph_nodes": ["crate:a", "module:a:crate::one", "module:a:crate::two", "file:a/src/one.rs"] },
    { "id": "cap.b", "graph_nodes": ["crate:b"] }
  ]
}"#;

/// Same meaning as `CATALOG`: reordered declarations, reordered and repeated node ids.
const CATALOG_REORDERED: &str = r#"{
  "schema": "chip.capabilities.v1",
  "capabilities": [
    { "id": "cap.b", "graph_nodes": ["crate:b", "crate:b"] },
    { "id": "cap.a", "graph_nodes": ["file:a/src/one.rs", "module:a:crate::two", "crate:a", "module:a:crate::one", "crate:a", "file:a/src/one.rs"] }
  ]
}"#;

struct Repo {
    root: PathBuf,
}

impl Repo {
    /// A copy of `slice_fixture` plus a second crate `b`, so an unrelated file can exist in
    /// the graph. Nothing under `crates/` is modified.
    fn new(name: &str, reverse_creation_order: bool) -> Repo {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../chip-graph/tests/slice_fixture");
        let root =
            std::env::temp_dir().join(format!("chip-slice-accept-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        copy_tree(&src, &root, reverse_creation_order);
        fs::remove_file(root.join("capabilities.json")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n",
        )
        .unwrap();
        fs::create_dir_all(root.join("b/src")).unwrap();
        fs::write(
            root.join("b/Cargo.toml"),
            "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(root.join("b/src/lib.rs"), "pub mod one;\n").unwrap();
        fs::write(root.join("b/src/one.rs"), "pub fn run() {}\n").unwrap();
        Repo { root }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn write_catalog(&self, text: &str) -> PathBuf {
        let path = self.path("capabilities.json");
        fs::write(&path, text).unwrap();
        path
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut full: Vec<String> = Vec::new();
        full.push(args[0].to_string());
        full.extend([
            "--root".to_string(),
            self.root.to_str().unwrap().to_string(),
        ]);
        full.extend(args[1..].iter().map(|a| a.to_string()));
        Command::new(env!("CARGO_BIN_EXE_chip-cli"))
            .args(&full)
            .output()
            .unwrap()
    }

    fn init(&self) -> Output {
        let out = self.run(&["init"]);
        assert!(out.status.success(), "{}", stderr(&out));
        out
    }

    fn slice(&self, catalog: &Path, extra: &[&str], capability: &str) -> Output {
        let mut args = vec!["slice", "--capabilities", catalog.to_str().unwrap()];
        args.extend_from_slice(extra);
        args.push(capability);
        self.run(&args)
    }

    /// Every file under the repository, with its bytes, as a comparable value.
    fn digest(&self) -> BTreeMap<String, Vec<u8>> {
        fn walk(dir: &Path, base: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, base, out);
                } else {
                    let rel = path
                        .strip_prefix(base)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned();
                    out.insert(rel, fs::read(&path).unwrap());
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(&self.root, &self.root, &mut out);
        out
    }

    fn snapshot_file(&self) -> PathBuf {
        let latest = fs::read_to_string(self.path(".chip/graph/latest")).unwrap();
        self.path(&format!(".chip/graph/{}.json", latest.trim()))
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn copy_tree(from: &Path, to: &Path, reverse: bool) {
    fs::create_dir_all(to).unwrap();
    let mut entries: Vec<_> = fs::read_dir(from).unwrap().map(|e| e.unwrap()).collect();
    entries.sort_by_key(|e| e.file_name());
    if reverse {
        entries.reverse();
    }
    for e in entries {
        let dest = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_tree(&e.path(), &dest, reverse);
        } else {
            fs::copy(e.path(), dest).unwrap();
        }
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// The line under `Heading:` — the token or the snapshot id.
fn section_value(output: &str, heading: &str) -> String {
    output
        .lines()
        .skip_while(|l| *l != format!("{heading}:"))
        .nth(1)
        .unwrap_or_else(|| panic!("no {heading} section in:\n{output}"))
        .trim()
        .to_string()
}

fn assert_no_token(out: &Output) {
    assert!(!out.status.success(), "must fail; stdout: {}", stdout(out));
    let text = stdout(out);
    for forbidden in ["StateToken", "sha256:", "Nodes:", "Edges:", "Capability:"] {
        assert!(
            !text.contains(forbidden),
            "failure must render no slice; stdout: {text}"
        );
    }
    assert!(
        !stderr(out).is_empty() || !text.is_empty(),
        "failure must explain itself"
    );
}

#[test]
fn slice_prints_the_contract_and_is_byte_identical_when_repeated() {
    let repo = Repo::new("contract", false);
    repo.init();
    let catalog = repo.write_catalog(CATALOG);

    let first = repo.slice(&catalog, &[], "cap.a");
    assert!(first.status.success(), "{}", stderr(&first));
    assert!(stderr(&first).is_empty());
    let text = stdout(&first);
    for heading in [
        "Capability:",
        "Snapshot:",
        "Nodes:",
        "Edges:",
        "StateToken:",
    ] {
        assert!(
            text.contains(&format!("\n{heading}\n")) || text.starts_with(&format!("{heading}\n")),
            "missing {heading} in\n{text}"
        );
    }
    assert!(text.starts_with("Capability:\n  cap.a\n"), "{text}");
    assert!(section_value(&text, "Snapshot").starts_with("sha256:"));
    let token = section_value(&text, "StateToken");
    assert!(token.starts_with("sha256:") && token.len() == 71, "{token}");
    assert!(!text.contains("Impact:"), "no changed paths were given");
    for node in [
        "crate:a",
        "file:a/src/one.rs",
        "module:a:crate::one",
        "module:a:crate::two",
    ] {
        assert!(
            text.contains(&format!("  {node}\n")),
            "{node} missing in\n{text}"
        );
    }
    // Only declared nodes: no crate b, no undeclared module.
    assert!(!text.contains("crate:b") && !text.contains("three"));

    let before = repo.digest();
    let second = repo.slice(&catalog, &[], "cap.a");
    assert_eq!(first.stdout, second.stdout, "output must be byte-identical");
    assert_eq!(before, repo.digest(), "slice must not mutate anything");
}

#[test]
fn impact_acceptance_exercises_the_catalog_and_the_slice_together() {
    let repo = Repo::new("impact", false);
    repo.init();
    let catalog = repo.write_catalog(CATALOG);
    let plain = stdout(&repo.slice(&catalog, &[], "cap.a"));

    let hit = repo.slice(&catalog, &["--changed", "a/src/one.rs"], "cap.a");
    assert!(hit.status.success(), "{}", stderr(&hit));
    let hit = stdout(&hit);
    assert!(hit.contains("Impact:\n  impacted\n"), "{hit}");
    assert!(!hit.contains("Unresolved:"));

    let miss = repo.slice(&catalog, &["--changed", "b/src/one.rs"], "cap.a");
    assert!(miss.status.success(), "{}", stderr(&miss));
    let miss = stdout(&miss);
    assert!(miss.contains("Impact:\n  unchanged\n"), "{miss}");
    assert!(
        !miss.contains("Unresolved:"),
        "b/src/one.rs is in the graph; it is resolved, not unknown"
    );

    // Impact never alters the slice or its token.
    for text in [&hit, &miss] {
        assert_eq!(
            section_value(text, "StateToken"),
            section_value(&plain, "StateToken")
        );
    }
    // The same path, given twice or with a prefix, is the same change.
    let dup = stdout(&repo.slice(
        &catalog,
        &[
            "--changed",
            "a/src/one.rs",
            "--changed",
            "file:a/src/one.rs",
        ],
        "cap.a",
    ));
    assert_eq!(dup, hit);
}

#[test]
fn an_unresolved_change_is_reported_and_is_not_the_same_as_unchanged() {
    let repo = Repo::new("unresolved", false);
    repo.init();
    let catalog = repo.write_catalog(CATALOG);

    let out = repo.slice(&catalog, &["--changed", "does/not/exist.rs"], "cap.a");
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("Unresolved:\n  does/not/exist.rs\n"),
        "{text}"
    );
    assert!(text.contains("StateToken:"));

    // A resolved unrelated change has no Unresolved section: the two cases are distinguishable.
    let resolved = stdout(&repo.slice(&catalog, &["--changed", "b/src/one.rs"], "cap.a"));
    assert!(!resolved.contains("Unresolved:"));

    // Mixed: the mapped path still counts, the unmapped one is still reported.
    let mixed = stdout(&repo.slice(
        &catalog,
        &[
            "--changed",
            "a/src/one.rs",
            "--changed",
            "does/not/exist.rs",
        ],
        "cap.a",
    ));
    assert!(
        mixed.contains("Impact:\n  impacted\n")
            && mixed.contains("Unresolved:\n  does/not/exist.rs\n")
    );
}

#[test]
fn slice_consumes_graph_state_and_never_creates_it() {
    let repo = Repo::new("missing-snapshot", false);
    let catalog = repo.write_catalog(CATALOG);
    let before = repo.digest();

    let out = repo.slice(&catalog, &[], "cap.a");
    assert!(!out.status.success());
    let text = stdout(&out);
    assert!(
        text.contains("No architecture snapshot found.") && text.contains("Run `chip init`."),
        "{text}"
    );
    assert!(!text.contains("StateToken"));
    assert!(!repo.path(".chip").exists(), "slice must not create .chip/");
    assert_eq!(before, repo.digest());

    // Also with --changed.
    let out = repo.slice(&catalog, &["--changed", "a/src/one.rs"], "cap.a");
    assert!(!out.status.success() && !repo.path(".chip").exists());
}

#[test]
fn a_missing_catalog_or_unknown_capability_emits_no_token() {
    let repo = Repo::new("missing-catalog", false);
    repo.init();
    let absent = repo.path("nope/capabilities.json");
    let before = repo.digest();

    let out = repo.slice(&absent, &[], "cap.a");
    assert!(!out.status.success());
    assert!(
        stdout(&out).contains("Capability catalog not found:"),
        "{}",
        stdout(&out)
    );
    assert!(!stdout(&out).contains("StateToken"));
    assert!(
        !repo.path("nope").exists() && !repo.path("capabilities.json").exists(),
        "no catalog is invented"
    );

    let catalog = repo.write_catalog(CATALOG);
    let before_with_catalog = {
        let mut d = before;
        d.insert("capabilities.json".into(), CATALOG.as_bytes().to_vec());
        d
    };
    let out = repo.slice(&catalog, &[], "cap.unknown");
    assert_no_token(&out);
    assert!(
        stderr(&out).contains("unknown capability: cap.unknown"),
        "{}",
        stderr(&out)
    );
    assert_eq!(before_with_catalog, repo.digest());
}

#[test]
fn an_invalid_catalog_renders_no_slice_and_no_token() {
    let repo = Repo::new("invalid-catalog", false);
    repo.init();
    let cases: [(&str, &str, &str); 7] = [
        (
            "wrong schema",
            r#"{"schema":"appport.capabilities.v1","capabilities":[]}"#,
            "schema",
        ),
        (
            "unknown graph node",
            r#"{"schema":"chip.capabilities.v1","capabilities":[{"id":"cap.a","graph_nodes":["file:does/not/exist.rs"]}]}"#,
            "not in the architecture snapshot",
        ),
        (
            "duplicate capability",
            r#"{"schema":"chip.capabilities.v1","capabilities":[{"id":"cap.a","graph_nodes":["crate:a"]},{"id":"cap.a","graph_nodes":["crate:b"]}]}"#,
            "duplicate capability id",
        ),
        (
            "empty id",
            r#"{"schema":"chip.capabilities.v1","capabilities":[{"id":"","graph_nodes":["crate:a"]}]}"#,
            "must not be empty",
        ),
        (
            "empty graph nodes",
            r#"{"schema":"chip.capabilities.v1","capabilities":[{"id":"cap.a","graph_nodes":[]}]}"#,
            "at least one graph node",
        ),
        ("not json", "{ definitely not json", "not valid JSON"),
        (
            "extra metadata",
            r#"{"schema":"chip.capabilities.v1","capabilities":[{"id":"cap.a","graph_nodes":["crate:a"],"description":"x"}]}"#,
            "malformed",
        ),
    ];
    for (name, text, expected) in cases {
        let catalog = repo.write_catalog(text);
        let before = repo.digest();
        for extra in [&[][..], &["--changed", "a/src/one.rs"][..]] {
            let out = repo.slice(&catalog, extra, "cap.a");
            assert_no_token(&out);
            assert!(
                stderr(&out).contains(expected),
                "{name}: expected {expected:?} in {}",
                stderr(&out)
            );
        }
        assert_eq!(
            before,
            repo.digest(),
            "{name}: slice must not mutate anything"
        );
    }
}

#[test]
fn a_tampered_snapshot_emits_no_token() {
    let repo = Repo::new("tampered", false);
    repo.init();
    let catalog = repo.write_catalog(CATALOG);
    let good = fs::read_to_string(repo.snapshot_file()).unwrap();
    assert!(repo.slice(&catalog, &[], "cap.a").status.success());

    for (name, corrupted) in [
        ("edited content", good.replace("crate:b", "crate:q")),
        ("truncated", good[..good.len() / 2].to_string()),
        ("garbage", "not a snapshot".to_string()),
        ("emptied", String::new()),
    ] {
        fs::write(repo.snapshot_file(), corrupted).unwrap();
        for extra in [&[][..], &["--changed", "a/src/one.rs"][..]] {
            let out = repo.slice(&catalog, extra, "cap.a");
            assert_no_token(&out);
            assert!(
                !stderr(&out).is_empty(),
                "{name}: the refusal must be explained"
            );
        }
    }
    // The refusal is about the snapshot, not the command: restoring it restores the output.
    fs::write(repo.snapshot_file(), good).unwrap();
    assert!(repo.slice(&catalog, &[], "cap.a").status.success());
}

#[test]
fn output_is_deterministic_across_ordering_and_location() {
    let repo = Repo::new("determinism-a", false);
    repo.init();
    let baseline = stdout(&repo.slice(&repo.write_catalog(CATALOG), &[], "cap.a"));

    // Reordered declarations and repeated node declarations.
    let reordered = stdout(&repo.slice(&repo.write_catalog(CATALOG_REORDERED), &[], "cap.a"));
    assert_eq!(reordered, baseline);

    // Different filesystem creation order, different directory name and location.
    let other = Repo::new("determinism-b-with-another-name", true);
    other.init();
    let elsewhere = stdout(&other.slice(&other.write_catalog(CATALOG_REORDERED), &[], "cap.a"));
    assert_eq!(elsewhere, baseline);
    assert_eq!(
        section_value(&elsewhere, "StateToken"),
        section_value(&baseline, "StateToken")
    );

    // Re-running init on an unchanged repository does not move anything.
    let again = repo.init();
    assert!(stdout(&again).contains(&section_value(&baseline, "Snapshot")));
    assert_eq!(
        stdout(&repo.slice(&repo.path("capabilities.json"), &[], "cap.a")),
        baseline
    );

    // No machine-specific text leaks into the output.
    assert!(!baseline.contains(repo.root.to_str().unwrap()) && !baseline.contains("/tmp/"));
}

#[test]
fn the_token_follows_relevant_structure_not_the_repository() {
    let repo = Repo::new("stability", false);
    repo.init();
    let catalog = repo.write_catalog(CATALOG);
    let first = stdout(&repo.slice(&catalog, &[], "cap.a"));
    let (t1, s1) = (
        section_value(&first, "StateToken"),
        section_value(&first, "Snapshot"),
    );

    // The repository changes, outside the capability's slice.
    fs::write(
        repo.path("a/src/three.rs"),
        "pub fn other() {}\npub fn more() {}\n",
    )
    .unwrap();
    fs::write(
        repo.path("b/src/one.rs"),
        "pub fn run() {}\npub struct Extra;\n",
    )
    .unwrap();
    fs::write(repo.path("a/src/extra.rs"), "pub fn extra() {}\n").unwrap();
    repo.init();
    let second = stdout(&repo.slice(&catalog, &[], "cap.a"));
    let (t2, s2) = (
        section_value(&second, "StateToken"),
        section_value(&second, "Snapshot"),
    );
    assert_ne!(s1, s2, "the architecture snapshot did change");
    assert_eq!(t1, t2, "the capability-relevant state did not");

    // A structural change inside the declared slice: one.rs now imports module two.
    fs::write(
        repo.path("a/src/one.rs"),
        "use crate::two::helper;\n\npub fn run() {\n    helper();\n}\n",
    )
    .unwrap();
    repo.init();
    let third = stdout(&repo.slice(&catalog, &[], "cap.a"));
    let t3 = section_value(&third, "StateToken");
    assert_ne!(
        t2, t3,
        "a changed edge inside the slice must change the token"
    );
    assert!(
        third.contains("  file:a/src/one.rs imports module:a:crate::two\n"),
        "{third}"
    );

    // A capability whose slice was not touched keeps its token through all of it.
    let b_after = section_value(&stdout(&repo.slice(&catalog, &[], "cap.b")), "StateToken");
    assert_eq!(b_before, b_after);
}
