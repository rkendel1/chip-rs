//! `project.read` / `project.write` against the real filesystem: what they do, what they refuse,
//! and what the audit invariants say about forged observations.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chip_core::{
    CapabilityId, CapabilityProvider, ExecutionError, ExecutionId, ExecutionRequest,
    ExecutionStatus, Executor, InputValue, Observation, ObservationKind,
};
use chip_project::{
    MAX_READ_BYTES, MAX_WRITE_BYTES, OUT_OF_ROOT_WRITE, PATH_ESCAPE, PROJECT_READ, PROJECT_WRITE,
    ProjectExecutor, out_of_root_write_invariant, path_escape_invariant, write_summary,
};

struct Fixture {
    root: PathBuf,
    outside: PathBuf,
}

fn fixture(tag: &str) -> Fixture {
    let base = std::env::temp_dir().join(format!("chip-project-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("project");
    let outside = base.join("outside");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn one() -> u32 { 1 }\n").unwrap();
    std::fs::write(outside.join("secret.txt"), "outside secret\n").unwrap();
    Fixture { root, outside }
}

fn inputs(pairs: &[(&str, &str)]) -> BTreeMap<String, InputValue> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), InputValue::Text(v.to_string())))
        .collect()
}

async fn run(
    project: &ProjectExecutor,
    capability: &str,
    given: BTreeMap<String, InputValue>,
) -> Result<chip_core::ExecutionResult, ExecutionError> {
    project
        .execute(ExecutionRequest::new(ExecutionId::new("e"), capability).with_inputs(given))
        .await
}

async fn read(
    p: &ProjectExecutor,
    path: &str,
) -> Result<chip_core::ExecutionResult, ExecutionError> {
    run(p, PROJECT_READ, inputs(&[("path", path)])).await
}

async fn write(
    p: &ProjectExecutor,
    path: &str,
    content: &str,
) -> Result<chip_core::ExecutionResult, ExecutionError> {
    run(
        p,
        PROJECT_WRITE,
        inputs(&[("path", path), ("content", content)]),
    )
    .await
}

fn first_line(r: &chip_core::ExecutionResult) -> serde_json::Value {
    serde_json::from_str(r.output.lines().next().unwrap()).unwrap()
}

fn no_temporaries(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .all(|e| !e.file_name().to_string_lossy().starts_with(".chip-write-"))
}

fn observation(kind: ObservationKind, output: &str) -> Observation {
    Observation {
        execution_id: ExecutionId::new("e"),
        kind,
        status: if kind == ObservationKind::ExecutionCompleted {
            ExecutionStatus::Success
        } else {
            ExecutionStatus::Failure
        },
        output: Some(output.to_string()),
        receipt_id: None,
    }
}

#[tokio::test]
async fn the_capabilities_are_declared_with_their_inputs_and_are_never_answered_from_memory() {
    let f = fixture("declared");
    let p = ProjectExecutor::new(&f.root);
    let found = p.capabilities().await.unwrap();
    let ids: Vec<&str> = found.iter().map(|d| d.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "project.list",
            "project.search",
            PROJECT_READ,
            PROJECT_WRITE
        ]
    );
    let names =
        |i: usize| -> Vec<&str> { found[i].inputs.iter().map(|x| x.name.as_str()).collect() };
    assert_eq!(names(0), ["path"]);
    assert_eq!(names(1), ["query", "path"]);
    assert_eq!(names(2), ["path"]);
    assert_eq!(names(3), ["path", "content"]);
    for d in &found {
        assert!(
            !d.reuse_evidence,
            "{} could be answered from stale evidence",
            d.id
        );
        let navigation = matches!(d.id.as_str(), "project.list" | "project.search");
        // Only the file-content capabilities declare room for a file; navigation takes short text.
        assert_eq!(
            d.max_input_bytes,
            (!navigation).then_some(MAX_WRITE_BYTES),
            "{}",
            d.id
        );
        assert!(
            navigation || d.inputs.iter().all(|i| i.required),
            "{}",
            d.id
        );
        let text = format!("{} {} {}", d.id, d.name, d.description).to_lowercase();
        for word in ["shell", "command", "cargo", "exec", "process"] {
            assert!(!text.contains(word), "{} mentions {word}", d.id);
        }
    }
}

#[tokio::test]
async fn read_returns_the_real_content_in_a_runtime_generated_observation() {
    let f = fixture("read");
    let p = ProjectExecutor::new(&f.root);
    let r = read(&p, "src/lib.rs").await.unwrap();
    assert_eq!(r.status, ExecutionStatus::Success);
    assert_eq!(r.receipt_id, None);
    let head = first_line(&r);
    assert_eq!(head["capability"], PROJECT_READ);
    assert_eq!(head["path"], "src/lib.rs");
    assert_eq!(head["bytes"], 26);
    assert!(
        r.output
            .ends_with("--- content ---\npub fn one() -> u32 { 1 }\n")
    );
    // No host path anywhere in what the model will see.
    assert!(!r.output.contains(f.root.to_str().unwrap()), "{}", r.output);
}

#[tokio::test]
async fn read_failures_are_observed_as_reality_and_change_nothing() {
    let f = fixture("read-fail");
    let p = ProjectExecutor::new(&f.root);
    std::fs::write(f.root.join("bin.dat"), [0xff, 0xfe, 0xfd]).unwrap();
    std::fs::write(f.root.join("big.txt"), vec![b'a'; MAX_READ_BYTES + 1]).unwrap();
    std::fs::write(f.root.join("exact.txt"), vec![b'a'; MAX_READ_BYTES]).unwrap();
    for (path, error) in [
        ("missing.rs", "not_found"),
        ("nodir/missing.rs", "not_found"),
        ("src", "not_a_file"),
        ("bin.dat", "not_utf8"),
        ("big.txt", "too_large"),
    ] {
        let r = read(&p, path).await.unwrap();
        assert_eq!(r.status, ExecutionStatus::Failure, "{path}");
        assert_eq!(first_line(&r)["error"], error, "{path}");
        assert!(!r.output.contains("--- content ---"), "{path}");
    }
    assert_eq!(
        read(&p, "exact.txt").await.unwrap().status,
        ExecutionStatus::Success
    );
}

#[tokio::test]
async fn write_creates_replaces_and_reports_what_the_filesystem_holds() {
    let f = fixture("write");
    let p = ProjectExecutor::new(&f.root);
    let r = write(&p, "src/new.rs", "pub fn two() -> u32 { 2 }\n")
        .await
        .unwrap();
    assert_eq!(r.status, ExecutionStatus::Success);
    let head = first_line(&r);
    assert_eq!(
        (
            head["operation"].as_str(),
            head["bytes_written"].as_u64(),
            head["changed"].as_bool()
        ),
        (Some("created"), Some(26), Some(true))
    );
    assert_eq!(r.receipt_id, None);
    assert_eq!(
        std::fs::read_to_string(f.root.join("src/new.rs")).unwrap(),
        "pub fn two() -> u32 { 2 }\n"
    );

    let r = write(&p, "src/lib.rs", "pub fn one() -> u32 { 11 }\n")
        .await
        .unwrap();
    assert_eq!(first_line(&r)["operation"], "replaced");
    assert_eq!(first_line(&r)["changed"], true);
    assert_eq!(
        std::fs::read_to_string(f.root.join("src/lib.rs")).unwrap(),
        "pub fn one() -> u32 { 11 }\n"
    );

    // The same content again is a successful write that changed nothing, and says so.
    let r = write(&p, "src/lib.rs", "pub fn one() -> u32 { 11 }\n")
        .await
        .unwrap();
    assert_eq!(first_line(&r)["changed"], false);
    assert!(no_temporaries(&f.root.join("src")));
    // Content with newlines, quotes, backslashes and non-ASCII text survives exactly.
    let tricky = "line1\n\t\"quoted\" \\ back\r\nünï ✓\n";
    write(&p, "src/tricky.rs", tricky).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(f.root.join("src/tricky.rs")).unwrap(),
        tricky
    );
    // Exactly the limit is accepted, and what lands is exactly that many bytes.
    let r = write(&p, "max.txt", &"a".repeat(MAX_WRITE_BYTES))
        .await
        .unwrap();
    assert_eq!(r.status, ExecutionStatus::Success);
    assert_eq!(
        std::fs::metadata(f.root.join("max.txt")).unwrap().len() as usize,
        MAX_WRITE_BYTES
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_replaced_file_keeps_its_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let f = fixture("mode");
    let p = ProjectExecutor::new(&f.root);
    let script = f.root.join("run.sh");
    std::fs::write(&script, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    write(&p, "run.sh", "#!/bin/sh\necho hi\n").await.unwrap();
    assert_eq!(
        std::fs::metadata(&script).unwrap().permissions().mode() & 0o777,
        0o755
    );
    // A new file is an ordinary one.
    write(&p, "plain.txt", "x").await.unwrap();
    assert_eq!(
        std::fs::metadata(f.root.join("plain.txt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o644
    );
}

#[tokio::test]
async fn write_failures_are_observed_failures_and_leave_no_partial_state() {
    let f = fixture("write-fail");
    let p = ProjectExecutor::new(&f.root);
    // A directory that does not exist is not created for the model.
    let r = write(&p, "nodir/x.rs", "x").await.unwrap();
    assert_eq!(
        (r.status, first_line(&r)["error"].as_str()),
        (ExecutionStatus::Failure, Some("parent_missing"))
    );
    assert!(!f.root.join("nodir").exists());
    // A directory is not a file.
    let r = write(&p, "src", "x").await.unwrap();
    assert_eq!(
        (r.status, first_line(&r)["error"].as_str()),
        (ExecutionStatus::Failure, Some("is_a_directory"))
    );
    assert!(f.root.join("src").is_dir());
}

#[cfg(unix)]
#[tokio::test]
async fn a_write_the_filesystem_refuses_changes_nothing_and_reports_no_success() {
    use std::os::unix::fs::PermissionsExt;
    let f = fixture("readonly");
    let p = ProjectExecutor::new(&f.root);
    let dir = f.root.join("src");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    let writable_anyway = std::fs::File::create(dir.join(".probe")).is_ok();
    let _ = std::fs::remove_file(dir.join(".probe"));
    let r = write(&p, "src/lib.rs", "pub fn nope() {}\n").await.unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    if writable_anyway {
        eprintln!("SKIPPED: this account can write to a read-only directory");
        return;
    }
    assert_eq!(r.status, ExecutionStatus::Failure, "{}", r.output);
    assert_eq!(first_line(&r)["error"], "permission_denied");
    assert_eq!(
        std::fs::read_to_string(dir.join("lib.rs")).unwrap(),
        "pub fn one() -> u32 { 1 }\n"
    );
    assert!(no_temporaries(&dir));
    assert!(
        !r.output.contains("bytes_written"),
        "no success fields on a failure"
    );
}

#[tokio::test]
async fn invalid_paths_are_refused_before_anything_happens_and_are_never_repaired() {
    let f = fixture("paths");
    let p = ProjectExecutor::new(&f.root);
    let long = "a/".repeat(101);
    let cases: Vec<(&str, String)> = vec![
        ("traversal", "../outside.txt".into()),
        ("nested traversal", "src/../../outside.txt".into()),
        ("deep traversal", "../../../../etc/passwd".into()),
        ("absolute", "/etc/passwd".into()),
        (
            "absolute to the root itself",
            format!("{}/x.txt", f.root.display()),
        ),
        ("dot component", "src/./lib.rs".into()),
        ("leading dot component", "./src/lib.rs".into()),
        ("empty component", "src//lib.rs".into()),
        ("trailing slash", "src/".into()),
        ("empty", "".into()),
        ("too long", long),
        ("space", "my file.rs".into()),
        ("backslash", "src\\lib.rs".into()),
        ("windows drive", "C:/Windows/x".into()),
        ("colon", "a:b".into()),
        ("non-ascii", "é.rs".into()),
        ("nul", "a\0b".into()),
        ("tilde", "~/x".into()),
        ("dollar", "$HOME/x".into()),
        ("reserved .git", ".git/config".into()),
        ("reserved nested .git", "src/.git/hooks".into()),
        ("reserved .env", ".env".into()),
        ("reserved .env.local", ".env.local".into()),
        ("reserved nested env", "src/.env.production".into()),
        ("reserved temp", ".chip-write-1-1.tmp".into()),
    ];
    let before = std::fs::read_dir(&f.root).unwrap().count();
    for (what, path) in &cases {
        let id = CapabilityId::new(PROJECT_WRITE).unwrap();
        let given = inputs(&[("path", path), ("content", "evil")]);
        assert!(
            p.validate_inputs(&id, &given).await.is_err(),
            "{what}: {path:?} was accepted"
        );
        // And the executor refuses it too, on its own: it does not rely on having been validated.
        assert!(
            matches!(
                run(&p, PROJECT_WRITE, given).await,
                Err(ExecutionError::InvalidRequest(_))
            ),
            "{what}"
        );
        let rid = CapabilityId::new(PROJECT_READ).unwrap();
        assert!(
            p.validate_inputs(&rid, &inputs(&[("path", path)]))
                .await
                .is_err(),
            "{what} (read)"
        );
        assert!(
            matches!(read(&p, path).await, Err(ExecutionError::InvalidRequest(_))),
            "{what} (read)"
        );
    }
    assert!(!f.root.parent().unwrap().join("outside.txt").exists());
    assert!(!f.root.join("outside.txt").exists());
    assert_eq!(
        std::fs::read_dir(&f.root).unwrap().count(),
        before,
        "the project root changed"
    );
    assert_eq!(
        std::fs::read_to_string(f.outside.join("secret.txt")).unwrap(),
        "outside secret\n"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_on_the_path_is_refused_whatever_it_points_at() {
    use std::os::unix::fs::symlink;
    let f = fixture("symlinks");
    let p = ProjectExecutor::new(&f.root);
    symlink(&f.outside, f.root.join("linkdir")).unwrap();
    symlink(f.outside.join("secret.txt"), f.root.join("alias.txt")).unwrap();
    symlink(f.root.join("src/lib.rs"), f.root.join("inside_alias.rs")).unwrap();
    symlink(f.outside.join("does-not-exist"), f.root.join("dangling")).unwrap();
    symlink(&f.outside, f.root.join("src/nested_link")).unwrap();
    for path in [
        "linkdir/x.txt",
        "linkdir/secret.txt",
        "alias.txt",
        "inside_alias.rs",
        "dangling",
        "src/nested_link/x.txt",
    ] {
        let given = inputs(&[("path", path), ("content", "pwned")]);
        assert!(
            matches!(
                run(&p, PROJECT_WRITE, given).await,
                Err(ExecutionError::InvalidRequest(_))
            ),
            "write {path}"
        );
        assert!(
            matches!(read(&p, path).await, Err(ExecutionError::InvalidRequest(_))),
            "read {path}"
        );
    }
    assert!(
        !f.outside.join("x.txt").exists(),
        "a write went through a symlink"
    );
    assert_eq!(
        std::fs::read_to_string(f.outside.join("secret.txt")).unwrap(),
        "outside secret\n"
    );
    assert!(!f.outside.join("does-not-exist").exists());
    // A symlink the model did not name is not followed either: the root itself may be reached
    // through one (that is Chip's own, resolved once), but nothing under it may.
    let via_link = ProjectExecutor::new({
        let l = f.root.parent().unwrap().join("root_link");
        let _ = std::fs::remove_file(&l);
        symlink(&f.root, &l).unwrap();
        l
    });
    assert_eq!(
        read(&via_link, "src/lib.rs").await.unwrap().status,
        ExecutionStatus::Success
    );
}

#[tokio::test]
async fn oversized_content_and_malformed_inputs_are_refused() {
    let f = fixture("inputs");
    let p = ProjectExecutor::new(&f.root);
    let big = "a".repeat(MAX_WRITE_BYTES + 1);
    assert!(matches!(
        write(&p, "big.txt", &big).await,
        Err(ExecutionError::InvalidRequest(_))
    ));
    assert!(!f.root.join("big.txt").exists());
    let bad: Vec<(&str, &str, BTreeMap<String, InputValue>)> = vec![
        (
            "write: no content",
            PROJECT_WRITE,
            inputs(&[("path", "a.txt")]),
        ),
        ("write: no path", PROJECT_WRITE, inputs(&[("content", "x")])),
        (
            "write: extra mode",
            PROJECT_WRITE,
            inputs(&[("path", "a.txt"), ("content", "x"), ("mode", "755")]),
        ),
        (
            "write: extra root",
            PROJECT_WRITE,
            inputs(&[("path", "a.txt"), ("content", "x"), ("root", "/")]),
        ),
        (
            "read: content",
            PROJECT_READ,
            inputs(&[("path", "a.txt"), ("content", "x")]),
        ),
        ("read: no path", PROJECT_READ, inputs(&[])),
        (
            "read: directory input",
            PROJECT_READ,
            inputs(&[("path", "a.txt"), ("directory", "/")]),
        ),
        (
            "read: integer path",
            PROJECT_READ,
            [("path".to_string(), InputValue::Integer(1))].into(),
        ),
        (
            "write: bool content",
            PROJECT_WRITE,
            [
                ("path".to_string(), InputValue::Text("a.txt".into())),
                ("content".to_string(), InputValue::Bool(true)),
            ]
            .into(),
        ),
    ];
    for (what, capability, given) in bad {
        assert!(
            matches!(
                run(&p, capability, given).await,
                Err(ExecutionError::InvalidRequest(_))
            ),
            "{what}"
        );
    }
    assert!(!f.root.join("a.txt").exists());
    // Anything but the two declared capabilities is not this executor's.
    for intent in ["project.delete", "project", "pax.test", "shell.exec", ""] {
        assert!(
            matches!(
                run(&p, intent, inputs(&[])).await,
                Err(ExecutionError::InvalidRequest(_))
            ),
            "{intent:?}"
        );
    }
}

#[tokio::test]
async fn a_missing_root_makes_the_capabilities_unavailable() {
    let p = ProjectExecutor::new(std::env::temp_dir().join("chip-project-no-such-root-xyz"));
    for id in [PROJECT_READ, PROJECT_WRITE] {
        let a = p.availability(&CapabilityId::new(id).unwrap()).await;
        assert!(
            matches!(a, chip_core::CapabilityAvailability::Unavailable(_)),
            "{id}"
        );
    }
    assert!(matches!(
        read(&p, "a").await,
        Err(ExecutionError::ExecutorUnavailable(_))
    ));
    let file = std::env::temp_dir().join(format!("chip-project-a-file-{}", std::process::id()));
    std::fs::write(&file, "x").unwrap();
    let p = ProjectExecutor::new(&file);
    assert!(matches!(
        p.availability(&CapabilityId::new(PROJECT_READ).unwrap())
            .await,
        chip_core::CapabilityAvailability::Unavailable(_)
    ));
}

// ---- the audit invariants judge recorded observations on their own ----------------------------------

#[tokio::test]
async fn the_invariants_hold_for_honest_observations() {
    let f = fixture("inv-honest");
    let p = ProjectExecutor::new(&f.root);
    let escape = path_escape_invariant(&f.root);
    let outside = out_of_root_write_invariant(&f.root);
    assert_eq!(
        (escape.name(), outside.name()),
        (PATH_ESCAPE, OUT_OF_ROOT_WRITE)
    );
    for r in [
        read(&p, "src/lib.rs").await.unwrap(),
        read(&p, "missing.rs").await.unwrap(),
        write(&p, "src/a.rs", "x").await.unwrap(),
        write(&p, "nodir/a.rs", "x").await.unwrap(),
    ] {
        let kind = if r.status == ExecutionStatus::Success {
            ObservationKind::ExecutionCompleted
        } else {
            ObservationKind::ExecutionFailed
        };
        let o = observation(kind, &r.output);
        assert_eq!(
            (escape.violations(&o), outside.violations(&o)),
            (0, 0),
            "{}",
            r.output
        );
    }
    // Observations that are not project observations are none of its business.
    let pax = observation(
        ObservationKind::ExecutionCompleted,
        "{\"schema\":\"pax.execution-result.v1\"}\nrest",
    );
    assert_eq!((escape.violations(&pax), outside.violations(&pax)), (0, 0));
    assert_eq!(
        escape.violations(&observation(
            ObservationKind::ExecutionCompleted,
            "not json"
        )),
        0
    );
}

#[cfg(unix)]
#[tokio::test]
async fn the_invariants_catch_forged_or_faulty_observations_independently_of_the_executor() {
    use std::os::unix::fs::symlink;
    let f = fixture("inv-forged");
    let escape = path_escape_invariant(&f.root);
    let outside = out_of_root_write_invariant(&f.root);
    let rec = |cap: &str, path: &str| {
        observation(
            ObservationKind::ExecutionCompleted,
            &format!(
                "{{\"bytes_written\":1,\"capability\":\"{cap}\",\"changed\":true,\"operation\":\"created\",\"path\":{}}}",
                serde_json::to_string(path).unwrap()
            ),
        )
    };
    // A path that is not project-relative, in any observation of a project capability.
    for bad in [
        "../x",
        "/etc/passwd",
        "a/../../b",
        "a//b",
        "./a",
        "",
        "a\\b",
    ] {
        assert_eq!(escape.violations(&rec(PROJECT_WRITE, bad)), 1, "{bad:?}");
        assert_eq!(escape.violations(&rec(PROJECT_READ, bad)), 1, "{bad:?}");
        assert_eq!(outside.violations(&rec(PROJECT_WRITE, bad)), 1, "{bad:?}");
    }
    // A failed observation of an escaping path still shows the executor was asked to touch it.
    let failed = observation(
        ObservationKind::ExecutionFailed,
        "{\"capability\":\"project.write\",\"error\":\"io_error\",\"path\":\"../x\"}",
    );
    assert_eq!(escape.violations(&failed), 1);
    assert_eq!(outside.violations(&failed), 0, "nothing was written");
    // A write that really landed outside the root through a symlink.
    symlink(&f.outside, f.root.join("linkdir")).unwrap();
    std::fs::write(f.outside.join("landed.txt"), "x").unwrap();
    assert_eq!(
        outside.violations(&rec(PROJECT_WRITE, "linkdir/landed.txt")),
        1
    );
    // A read that really came from outside.
    assert_eq!(
        escape.violations(&rec(PROJECT_READ, "linkdir/landed.txt")),
        1
    );
    // A success claimed for a file that is not there cannot be shown to be inside: fail closed.
    assert_eq!(
        outside.violations(&rec(PROJECT_WRITE, "src/never_written.rs")),
        1
    );
    // And an honest one is clean.
    assert_eq!(outside.violations(&rec(PROJECT_WRITE, "src/lib.rs")), 0);
}

#[tokio::test]
async fn write_summary_reads_only_successful_project_writes() {
    let f = fixture("summary");
    let p = ProjectExecutor::new(&f.root);
    let ok = write(&p, "src/s.rs", "x").await.unwrap();
    assert_eq!(
        write_summary(&observation(
            ObservationKind::ExecutionCompleted,
            &ok.output
        )),
        Some(("src/s.rs".into(), true))
    );
    let same = write(&p, "src/s.rs", "x").await.unwrap();
    assert_eq!(
        write_summary(&observation(
            ObservationKind::ExecutionCompleted,
            &same.output
        )),
        Some(("src/s.rs".into(), false))
    );
    assert_eq!(
        write_summary(&observation(ObservationKind::ExecutionFailed, &ok.output)),
        None
    );
    let r = read(&p, "src/s.rs").await.unwrap();
    assert_eq!(
        write_summary(&observation(ObservationKind::ExecutionCompleted, &r.output)),
        None
    );
}
