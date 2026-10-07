//! `project.list` and `project.search` against the real filesystem: what they report, what they
//! refuse, the bounds they keep, and what the audit invariants say about forged observations.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chip_core::{
    CapabilityId, CapabilityProvider, ExecutionError, ExecutionId, ExecutionRequest,
    ExecutionStatus, Executor, InputValue, Observation, ObservationKind,
};
use chip_project::{
    HOST_PATH_LEAK, MAX_LIST_ENTRIES, MAX_MATCHES, MAX_OUTPUT_BYTES, MAX_QUERY_BYTES,
    MAX_SEARCH_FILES, NAVIGATION_MISMATCH, PATH_ESCAPE, ProjectExecutor, host_path_leak_invariant,
    navigation_mismatch_invariant, path_escape_invariant,
};

struct Fixture {
    root: PathBuf,
    outside: PathBuf,
}

fn fixture(tag: &str) -> Fixture {
    let base = std::env::temp_dir().join(format!("chip-nav-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let (root, outside) = (base.join("project"), base.join("outside"));
    for d in ["src", "tests", "docs", ".git", "target/debug"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn alpha() {}\npub fn beta() {}\n// TODO: gamma\n",
    )
    .unwrap();
    std::fs::write(root.join("src/util.rs"), "pub fn helper() { /* beta */ }\n").unwrap();
    std::fs::write(root.join("tests/a.rs"), "fn t() { beta(); }\n").unwrap();
    std::fs::write(root.join("docs/notes.txt"), "beta notes\r\nsecond line\r\n").unwrap();
    std::fs::write(root.join(".git/config"), "beta = secret\n").unwrap();
    std::fs::write(root.join(".env"), "beta=secret\n").unwrap();
    std::fs::write(root.join("target/debug/out.txt"), "beta build output\n").unwrap();
    std::fs::write(outside.join("secret.txt"), "beta outside\n").unwrap();
    Fixture { root, outside }
}

fn inputs(pairs: &[(&str, &str)]) -> BTreeMap<String, InputValue> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), InputValue::Text(v.to_string())))
        .collect()
}

async fn run(
    p: &ProjectExecutor,
    capability: &str,
    given: BTreeMap<String, InputValue>,
) -> Result<chip_core::ExecutionResult, ExecutionError> {
    p.execute(ExecutionRequest::new(ExecutionId::new("e"), capability).with_inputs(given))
        .await
}

async fn list(
    p: &ProjectExecutor,
    path: Option<&str>,
) -> Result<chip_core::ExecutionResult, ExecutionError> {
    run(
        p,
        "project.list",
        path.map_or_else(BTreeMap::new, |p| inputs(&[("path", p)])),
    )
    .await
}

async fn search(
    p: &ProjectExecutor,
    query: &str,
    path: Option<&str>,
) -> Result<chip_core::ExecutionResult, ExecutionError> {
    let mut given = inputs(&[("query", query)]);
    if let Some(path) = path {
        given.extend(inputs(&[("path", path)]));
    }
    run(p, "project.search", given).await
}

fn head(r: &chip_core::ExecutionResult) -> serde_json::Value {
    serde_json::from_str(r.output.lines().next().unwrap()).unwrap()
}

fn body(r: &chip_core::ExecutionResult) -> Vec<String> {
    r.output.lines().skip(2).map(str::to_string).collect()
}

fn observation(r: &chip_core::ExecutionResult) -> Observation {
    Observation {
        execution_id: ExecutionId::new("e"),
        kind: if r.status == ExecutionStatus::Success {
            ObservationKind::ExecutionCompleted
        } else {
            ObservationKind::ExecutionFailed
        },
        status: r.status,
        output: Some(r.output.clone()),
        receipt_id: None,
    }
}

#[tokio::test]
async fn list_shows_the_real_entries_with_project_relative_paths_only() {
    let f = fixture("list");
    let p = ProjectExecutor::new(&f.root);
    let size = |rel: &str| std::fs::metadata(f.root.join(rel)).unwrap().len();
    for root_spelling in [None, Some(".")] {
        let r = list(&p, root_spelling).await.unwrap();
        assert_eq!(r.status, ExecutionStatus::Success);
        assert_eq!(head(&r)["path"], ".");
        // Sorted; nothing reserved, no symlinks; `target` is real and so it is shown.
        let expected: Vec<String> = vec![
            format!("file Cargo.toml {}", size("Cargo.toml")),
            "dir docs".into(),
            "dir src".into(),
            "dir target".into(),
            "dir tests".into(),
        ];
        assert_eq!(body(&r), expected);
        assert!(!r.output.contains(f.root.to_str().unwrap()), "{}", r.output);
    }
    let r = list(&p, Some("src")).await.unwrap();
    let expected: Vec<String> = vec![
        format!("file src/lib.rs {}", size("src/lib.rs")),
        format!("file src/util.rs {}", size("src/util.rs")),
    ];
    assert_eq!(body(&r), expected);
    assert_eq!(
        (
            head(&r)["entries"].as_u64(),
            head(&r)["truncated"].as_bool()
        ),
        (Some(2), Some(false))
    );
    assert_eq!(r.receipt_id, None);
    // Reality's answers, not refusals: a file is not a directory, a missing one is missing.
    for (path, error) in [
        ("Cargo.toml", "not_a_directory"),
        ("nothing", "not_found"),
        ("src/nothing/deeper", "not_found"),
    ] {
        let r = list(&p, Some(path)).await.unwrap();
        assert_eq!(
            (r.status, head(&r)["error"].as_str()),
            (ExecutionStatus::Failure, Some(error)),
            "{path}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn list_never_follows_or_shows_a_symlink() {
    use std::os::unix::fs::symlink;
    let f = fixture("list-links");
    symlink(&f.outside, f.root.join("linkdir")).unwrap();
    symlink(f.outside.join("secret.txt"), f.root.join("alias.txt")).unwrap();
    std::fs::write(f.root.join("has space.rs"), "x").unwrap();
    let p = ProjectExecutor::new(&f.root);
    let r = list(&p, None).await.unwrap();
    assert!(
        !r.output.contains("linkdir")
            && !r.output.contains("alias.txt")
            && !r.output.contains("space"),
        "{}",
        r.output
    );
    assert_eq!(
        (
            head(&r)["skipped_symlinks"].as_u64(),
            head(&r)["skipped_unaddressable"].as_u64()
        ),
        // Two symlinks; the reserved .git and .env and the unaddressable name make three.
        (Some(2), Some(3))
    );
    assert!(!r.output.contains(f.outside.to_str().unwrap()));
    for path in ["linkdir", "linkdir/sub"] {
        assert!(
            matches!(
                list(&p, Some(path)).await,
                Err(ExecutionError::InvalidRequest(_))
            ),
            "{path}"
        );
    }
}

#[tokio::test]
async fn list_is_bounded() {
    let f = fixture("list-big");
    std::fs::create_dir_all(f.root.join("many")).unwrap();
    for i in 0..(MAX_LIST_ENTRIES + 25) {
        std::fs::write(f.root.join(format!("many/f{i:04}.txt")), "x").unwrap();
    }
    let p = ProjectExecutor::new(&f.root);
    let r = list(&p, Some("many")).await.unwrap();
    assert_eq!(head(&r)["entries"], MAX_LIST_ENTRIES);
    assert_eq!(head(&r)["truncated"], true);
    assert_eq!(body(&r).len(), MAX_LIST_ENTRIES);
    assert!(r.output.len() <= MAX_OUTPUT_BYTES + 512);
    // What it did return is real, and the audit agrees.
    assert_eq!(
        navigation_mismatch_invariant(&f.root).violations_in_trajectory(&[observation(&r)]),
        0
    );
}

#[tokio::test]
async fn search_finds_literal_text_with_paths_and_line_numbers() {
    let f = fixture("search");
    let p = ProjectExecutor::new(&f.root);
    let r = search(&p, "beta", None).await.unwrap();
    assert_eq!(r.status, ExecutionStatus::Success);
    // Deterministic order; .git, .env and target never searched; CRLF lines come back clean.
    assert_eq!(
        body(&r),
        [
            "docs/notes.txt:1: beta notes",
            "src/lib.rs:2: pub fn beta() {}",
            "src/util.rs:1: pub fn helper() { /* beta */ }",
            "tests/a.rs:1: fn t() { beta(); }"
        ]
    );
    assert_eq!(
        (
            head(&r)["matches"].as_u64(),
            head(&r)["files_examined"].as_u64(),
            head(&r)["truncated"].as_bool()
        ),
        (Some(4), Some(5), Some(false))
    );
    assert!(
        !r.output.contains("secret")
            && !r.output.contains("outside")
            && !r.output.contains("build output")
    );
    assert!(!r.output.contains(f.root.to_str().unwrap()));
    // Narrowed to a directory, and to one file.
    assert_eq!(
        body(&search(&p, "beta", Some("src")).await.unwrap()),
        [
            "src/lib.rs:2: pub fn beta() {}",
            "src/util.rs:1: pub fn helper() { /* beta */ }"
        ]
    );
    assert_eq!(
        body(&search(&p, "gamma", Some("src/lib.rs")).await.unwrap()),
        ["src/lib.rs:3: // TODO: gamma"]
    );
    // A literal, not a pattern, and case-sensitive.
    assert!(body(&search(&p, "b.ta", None).await.unwrap()).is_empty());
    assert!(body(&search(&p, "BETA", None).await.unwrap()).is_empty());
    assert_eq!(
        body(&search(&p, "fn t() { beta(); }", None).await.unwrap()).len(),
        1
    );
    // No match is a real answer, not a failure.
    let none = search(&p, "zzzz-not-there", None).await.unwrap();
    assert_eq!(
        (none.status, head(&none)["matches"].as_u64()),
        (ExecutionStatus::Success, Some(0))
    );
    for (path, error) in [("nothing", "not_found")] {
        let r = search(&p, "beta", Some(path)).await.unwrap();
        assert_eq!(
            (r.status, head(&r)["error"].as_str()),
            (ExecutionStatus::Failure, Some(error))
        );
    }
}

#[tokio::test]
async fn search_skips_binary_and_oversized_files_and_says_so() {
    let f = fixture("search-skip");
    std::fs::write(
        f.root.join("blob.bin"),
        [b'b', b'e', b't', b'a', 0xff, 0xfe],
    )
    .unwrap();
    std::fs::write(f.root.join("huge.txt"), "beta ".repeat(100_000)).unwrap();
    let p = ProjectExecutor::new(&f.root);
    let r = search(&p, "beta", None).await.unwrap();
    assert_eq!(
        (
            head(&r)["skipped_non_utf8"].as_u64(),
            head(&r)["skipped_large"].as_u64()
        ),
        (Some(1), Some(1))
    );
    assert!(!r.output.contains("blob.bin") && !r.output.contains("huge.txt"));
}

#[tokio::test]
async fn search_is_bounded_in_matches_files_and_bytes() {
    let f = fixture("search-bounds");
    std::fs::create_dir_all(f.root.join("lots")).unwrap();
    std::fs::write(
        f.root.join("lots/a.txt"),
        "needle\n".repeat(MAX_MATCHES + 30),
    )
    .unwrap();
    let p = ProjectExecutor::new(&f.root);
    let r = search(&p, "needle", None).await.unwrap();
    assert_eq!(
        (
            head(&r)["matches"].as_u64(),
            head(&r)["truncated"].as_bool()
        ),
        (Some(MAX_MATCHES as u64), Some(true))
    );
    // Long lines are shown clipped, and the whole observation stays under its cap.
    std::fs::write(
        f.root.join("lots/long.txt"),
        format!("needle {}\n", "x".repeat(5000)),
    )
    .unwrap();
    let r = search(&p, "needle", Some("lots/long.txt")).await.unwrap();
    assert!(body(&r)[0].chars().count() < 260, "{}", body(&r)[0]);
    // More files than may be examined: stops, and says it was cut short.
    let f = fixture("search-files");
    std::fs::create_dir_all(f.root.join("pile")).unwrap();
    for i in 0..(MAX_SEARCH_FILES + 40) {
        std::fs::write(f.root.join(format!("pile/f{i:04}.txt")), "x\n").unwrap();
    }
    let r = search(&ProjectExecutor::new(&f.root), "needle", Some("pile"))
        .await
        .unwrap();
    assert_eq!(
        (
            head(&r)["files_examined"].as_u64(),
            head(&r)["truncated"].as_bool()
        ),
        (Some(MAX_SEARCH_FILES as u64), Some(true))
    );
}

#[tokio::test]
async fn navigation_requests_that_could_escape_are_refused_before_anything_happens() {
    let f = fixture("refuse");
    let p = ProjectExecutor::new(&f.root);
    let big_query = "q".repeat(MAX_QUERY_BYTES + 1);
    let bad_paths = [
        "../outside",
        "/etc",
        "src/../..",
        "./src",
        "src//x",
        "",
        ".git",
        ".git/hooks",
        ".env",
        "src/.env.local",
        "my dir",
        "~",
        "a\\b",
        "/",
    ];
    for path in bad_paths {
        assert!(
            matches!(
                list(&p, Some(path)).await,
                Err(ExecutionError::InvalidRequest(_))
            ),
            "list {path:?}"
        );
        assert!(
            matches!(
                search(&p, "beta", Some(path)).await,
                Err(ExecutionError::InvalidRequest(_))
            ),
            "search {path:?}"
        );
    }
    for query in ["", big_query.as_str(), "two\nlines", "tab\there", "nul\0"] {
        let r = search(&p, query, None).await;
        // A tab is a control character here too: the query is one plain line.
        assert!(
            matches!(r, Err(ExecutionError::InvalidRequest(_))),
            "{query:?}"
        );
    }
    let bad_shapes: Vec<(&str, &str, BTreeMap<String, InputValue>)> = vec![
        (
            "list: extra input",
            "project.list",
            inputs(&[("path", "src"), ("recursive", "yes")]),
        ),
        (
            "list: command",
            "project.list",
            inputs(&[("command", "ls")]),
        ),
        (
            "list: integer path",
            "project.list",
            [("path".to_string(), InputValue::Integer(1))].into(),
        ),
        (
            "search: no query",
            "project.search",
            inputs(&[("path", "src")]),
        ),
        (
            "search: extra input",
            "project.search",
            inputs(&[("query", "x"), ("regex", "true")]),
        ),
        (
            "search: bool query",
            "project.search",
            [("query".to_string(), InputValue::Bool(true))].into(),
        ),
        (
            "search: integer path",
            "project.search",
            [
                ("query".to_string(), InputValue::Text("x".into())),
                ("path".to_string(), InputValue::Integer(3)),
            ]
            .into(),
        ),
    ];
    for (what, capability, given) in bad_shapes {
        assert!(
            matches!(
                run(&p, capability, given.clone()).await,
                Err(ExecutionError::InvalidRequest(_))
            ),
            "{what}"
        );
        let id = CapabilityId::new(capability).unwrap();
        assert!(
            p.validate_inputs(&id, &given).await.is_err(),
            "{what} (validate)"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn search_refuses_symlinks_and_never_reads_through_one() {
    use std::os::unix::fs::symlink;
    let f = fixture("search-links");
    symlink(&f.outside, f.root.join("linkdir")).unwrap();
    symlink(f.outside.join("secret.txt"), f.root.join("alias.txt")).unwrap();
    symlink(&f.outside, f.root.join("src/nested")).unwrap();
    let p = ProjectExecutor::new(&f.root);
    for path in ["linkdir", "alias.txt", "src/nested", "linkdir/secret.txt"] {
        assert!(
            matches!(
                search(&p, "beta", Some(path)).await,
                Err(ExecutionError::InvalidRequest(_))
            ),
            "{path}"
        );
    }
    // Searching around them: a symlink inside a searched tree is not followed and not mentioned.
    let r = search(&p, "outside", None).await.unwrap();
    assert!(body(&r).is_empty(), "{}", r.output);
    assert!(!r.output.contains("secret.txt"));
}

#[tokio::test]
async fn the_audit_accepts_honest_navigation_and_catches_forgery() {
    let f = fixture("audit");
    let p = ProjectExecutor::new(&f.root);
    let nav = navigation_mismatch_invariant(&f.root);
    let escape = path_escape_invariant(&f.root);
    let leak = host_path_leak_invariant(&f.root);
    assert_eq!(
        (nav.name(), leak.name(), escape.name()),
        (NAVIGATION_MISMATCH, HOST_PATH_LEAK, PATH_ESCAPE)
    );
    let honest = [
        observation(&list(&p, None).await.unwrap()),
        observation(&list(&p, Some("src")).await.unwrap()),
        observation(&search(&p, "beta", None).await.unwrap()),
        observation(&search(&p, "nothing-here", Some("src")).await.unwrap()),
    ];
    assert_eq!(nav.violations_in_trajectory(&honest), 0);
    assert!(
        honest
            .iter()
            .all(|o| escape.violations(o) == 0 && leak.violations(o) == 0)
    );
    let forge = |text: &str| Observation {
        output: Some(text.to_string()),
        ..honest[0].clone()
    };
    // A listing that names a file that is not there, or omits one that is.
    let invented = forge(
        "{\"capability\":\"project.list\",\"entries\":1,\"path\":\".\",\"truncated\":false}\n--- entries ---\nfile ghost.rs 10",
    );
    assert_eq!(nav.violations_in_trajectory(&[invented]), 1);
    let omitted = forge(
        "{\"capability\":\"project.list\",\"entries\":0,\"path\":\"src\",\"truncated\":false}\n--- entries ---",
    );
    assert_eq!(nav.violations_in_trajectory(&[omitted]), 1);
    // A search that reports a match that is not in the file, or at a line that does not hold it.
    for row in [
        "src/lib.rs:2: pub fn delta() {}",
        "src/lib.rs:99: pub fn beta() {}",
        "src/ghost.rs:1: beta",
    ] {
        let invented = forge(&format!(
            "{{\"capability\":\"project.search\",\"query\":\"beta\",\"path\":\".\",\"matches\":1}}\n--- matches ---\n{row}"
        ));
        assert_eq!(nav.violations_in_trajectory(&[invented]), 1, "{row}");
    }
    // Paths that are not project-relative, anywhere in an observation.
    for bad in ["file /etc/passwd 1", "dir ../outside"] {
        let o = forge(&format!(
            "{{\"capability\":\"project.list\",\"entries\":1,\"path\":\".\",\"truncated\":false}}\n--- entries ---\n{bad}"
        ));
        assert!(escape.violations(&o) >= 1, "{bad}");
    }
    let o = forge(
        "{\"capability\":\"project.search\",\"query\":\"x\",\"path\":\"../outside\",\"matches\":0}\n--- matches ---",
    );
    assert_eq!(escape.violations(&o), 1);
    // A host path in what the model would be shown.
    let leaky = forge(&format!(
        "{{\"capability\":\"project.list\",\"entries\":1,\"path\":\".\",\"truncated\":false}}\n--- entries ---\nfile {}/x 1",
        f.root.display()
    ));
    assert_eq!(leak.violations(&leaky), 1);
    // Only project observations are judged by it.
    let pax = forge(&format!(
        "{{\"schema\":\"pax.execution-result.v1\"}}\nCompiling x ({})",
        f.root.display()
    ));
    assert_eq!(leak.violations(&pax), 0);
}

#[tokio::test]
async fn a_later_write_outdates_an_earlier_listing_without_making_it_a_forgery() {
    let f = fixture("audit-write");
    let p = ProjectExecutor::new(&f.root);
    let nav = navigation_mismatch_invariant(&f.root);
    let before = observation(&list(&p, Some("src")).await.unwrap());
    let found = observation(&search(&p, "beta", Some("src")).await.unwrap());
    let w = run(
        &p,
        "project.write",
        inputs(&[("path", "src/lib.rs"), ("content", "pub fn changed() {}\n")]),
    )
    .await
    .unwrap();
    let w2 = run(
        &p,
        "project.write",
        inputs(&[("path", "src/new.rs"), ("content", "x")]),
    )
    .await
    .unwrap();
    let trajectory = [
        before.clone(),
        found.clone(),
        observation(&w),
        observation(&w2),
    ];
    assert_eq!(
        nav.violations_in_trajectory(&trajectory),
        0,
        "a legitimately outdated observation was called a forgery"
    );
    // Without the writes in the trajectory, the same observations no longer match reality.
    assert!(nav.violations_in_trajectory(&[before, found]) >= 1);
}

#[tokio::test]
async fn a_later_test_run_may_change_the_project_so_only_existence_is_checked() {
    let f = fixture("audit-foreign");
    let p = ProjectExecutor::new(&f.root);
    let nav = navigation_mismatch_invariant(&f.root);
    let listed = observation(&list(&p, None).await.unwrap());
    let searched = observation(&search(&p, "beta", None).await.unwrap());
    // Something that is not a project capability ran afterwards, and created and changed files.
    let foreign = Observation {
        output: Some("{\"schema\":\"pax.execution-result.v1\"}\nran".into()),
        ..listed.clone()
    };
    std::fs::write(f.root.join("created_by_the_test_run.txt"), "x").unwrap();
    std::fs::write(f.root.join("src/util.rs"), "changed by the test run\n").unwrap();
    assert_eq!(
        nav.violations_in_trajectory(&[listed.clone(), searched.clone(), foreign.clone()]),
        0,
        "an honest observation was called a forgery"
    );
    // Still, what it claimed must exist: an entry that was never there is caught even so.
    let ghost = Observation {
        output: Some("{\"capability\":\"project.list\",\"entries\":1,\"path\":\".\",\"truncated\":false}\n--- entries ---\nfile ghost.rs 1".into()),
        ..listed.clone()
    };
    assert_eq!(nav.violations_in_trajectory(&[ghost, foreign.clone()]), 1);
    let ghost_match = Observation {
        output: Some("{\"capability\":\"project.search\",\"query\":\"beta\",\"path\":\".\",\"matches\":1}\n--- matches ---\nsrc/ghost.rs:1: beta".into()),
        ..searched
    };
    assert_eq!(nav.violations_in_trajectory(&[ghost_match, foreign]), 1);
}
