//! The read-only Git capabilities against real repositories (real `git`, real files): what they
//! observe, what they refuse, the bounds they keep, that they change nothing, and what the audit
//! invariants say about forged observations.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use chip_core::{
    CapabilityId, CapabilityProvider, ExecutionError, ExecutionId, ExecutionRequest,
    ExecutionStatus, Executor, InputValue, Observation, ObservationKind,
};
use chip_project::{
    DEFAULT_GIT_LOG_COUNT, GIT_CAPABILITIES, GIT_OBSERVATION_INVALID, GIT_SCOPE, HOST_PATH_LEAK,
    MAX_GIT_DIFF_BYTES, MAX_GIT_LOG_COUNT, PROJECT_GIT_DIFF, PROJECT_GIT_DIFF_STAT,
    PROJECT_GIT_LOG, PROJECT_GIT_STATUS, ProjectExecutor, git_observation_invariant,
    git_scope_invariant, host_path_leak_invariant,
};
use serde_json::Value;

fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("chip-git-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A repository with three commits and `a.txt`, `b.txt`, `c.txt`, `src/lib.rs`, all clean.
fn repo(tag: &str) -> PathBuf {
    let root = scratch(tag);
    git(&root, &["init", "-q", "-b", "main"]);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("a.txt"), "one\ntwo\nthree\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "first"]);
    std::fs::write(root.join("b.txt"), "bee\n").unwrap();
    std::fs::write(root.join("c.txt"), "sea\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "second"]);
    std::fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "third"]);
    root
}

/// The user's own uncommitted work: a modified file (unstaged), a new staged file, an untracked
/// file and a deleted file. `b.txt` is untouched.
fn make_dirty(root: &Path) {
    std::fs::write(root.join("a.txt"), "one\nTWO\nthree\nfour\n").unwrap();
    std::fs::write(root.join("staged.txt"), "staged\n").unwrap();
    git(root, &["add", "staged.txt"]);
    std::fs::write(root.join("untracked.txt"), "loose\n").unwrap();
    std::fs::remove_file(root.join("c.txt")).unwrap();
}

fn inputs(pairs: &[(&str, InputValue)]) -> BTreeMap<String, InputValue> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
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

async fn observe(p: &ProjectExecutor, capability: &str) -> (bool, String) {
    let r = run(p, capability, BTreeMap::new()).await.unwrap();
    (r.status == ExecutionStatus::Success, r.output)
}

fn first_line(text: &str) -> Value {
    serde_json::from_str(text.lines().next().unwrap()).unwrap()
}

fn list(v: &Value) -> Vec<&str> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap())
        .collect()
}

fn observation(kind: ObservationKind, text: &str) -> Observation {
    Observation {
        execution_id: ExecutionId::new("e"),
        kind,
        status: if kind == ObservationKind::ExecutionCompleted {
            ExecutionStatus::Success
        } else {
            ExecutionStatus::Failure
        },
        output: Some(text.to_string()),
        receipt_id: None,
    }
}

/// Every file under `dir` (relative path -> bytes), for proving nothing changed.
fn snapshot(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(base, &path, out);
            } else {
                out.insert(
                    path.strip_prefix(base)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    std::fs::read(&path).unwrap(),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

// ---- declarations -------------------------------------------------------------------------------------

#[tokio::test]
async fn exactly_four_read_only_git_capabilities_are_declared_deterministically() {
    let root = repo("declared");
    let p = ProjectExecutor::new(&root);
    let first = p.capabilities().await.unwrap();
    let second = p.capabilities().await.unwrap();
    assert_eq!(
        first.iter().map(|d| (&d.id, &d.inputs)).collect::<Vec<_>>(),
        second
            .iter()
            .map(|d| (&d.id, &d.inputs))
            .collect::<Vec<_>>(),
        "deterministic"
    );
    let git_ids: Vec<&str> = first
        .iter()
        .map(|d| d.id.as_str())
        .filter(|i| i.contains("git"))
        .collect();
    assert_eq!(
        git_ids,
        [
            PROJECT_GIT_STATUS,
            PROJECT_GIT_DIFF,
            PROJECT_GIT_DIFF_STAT,
            PROJECT_GIT_LOG
        ]
    );
    assert_eq!(GIT_CAPABILITIES.len(), 4);
    for d in &first {
        for mutation in [
            "add", "commit", "push", "pull", "fetch", "checkout", "switch", "reset", "restore",
            "clean", "merge", "rebase", "stash", "branch", "exec", "command",
        ] {
            assert!(
                !d.id.as_str().split('.').any(|part| part == mutation),
                "{} looks like a mutation",
                d.id
            );
        }
        assert!(
            d.description.chars().count() <= 160,
            "{} is cut when shown to the model",
            d.id
        );
        assert!(
            !d.reuse_evidence,
            "{} must never be answered from memory",
            d.id
        );
    }
    // Inputs match the implementation: three take none, the log takes an optional count.
    for d in first.iter().filter(|d| d.id.as_str().contains("git")) {
        match d.id.as_str() {
            PROJECT_GIT_LOG => {
                assert_eq!(d.inputs.len(), 1);
                assert_eq!(
                    (d.inputs[0].name.as_str(), d.inputs[0].required),
                    ("count", false)
                );
            }
            _ => assert!(d.inputs.is_empty(), "{}", d.id),
        }
    }
    for id in GIT_CAPABILITIES {
        let ready = p.availability(&CapabilityId::new(id).unwrap()).await;
        assert!(
            matches!(ready, chip_core::CapabilityAvailability::Available),
            "{id}"
        );
    }
}

// ---- status -------------------------------------------------------------------------------------------

#[tokio::test]
async fn status_of_a_clean_repository_is_clean_on_its_branch() {
    let root = repo("clean");
    let (ok, text) = observe(&ProjectExecutor::new(&root), PROJECT_GIT_STATUS).await;
    assert!(ok, "{text}");
    let v = first_line(&text);
    assert_eq!(
        (v["clean"].clone(), v["branch"].clone()),
        (true.into(), "main".into())
    );
    assert_eq!(v["repository"], ".");
    for k in ["staged", "unstaged", "untracked", "deleted", "unmerged"] {
        assert!(list(&v[k]).is_empty(), "{k}");
    }
    assert!(!text.contains(root.to_str().unwrap()), "no host path");
}

#[tokio::test]
async fn status_observes_a_dirty_tree_exactly_as_it_is() {
    let root = repo("dirty");
    make_dirty(&root);
    let (ok, text) = observe(&ProjectExecutor::new(&root), PROJECT_GIT_STATUS).await;
    assert!(ok, "{text}");
    let v = first_line(&text);
    assert_eq!(v["clean"], false);
    assert_eq!(list(&v["staged"]), ["staged.txt"]);
    assert_eq!(list(&v["unstaged"]), ["a.txt", "c.txt"]);
    assert_eq!(list(&v["untracked"]), ["untracked.txt"]);
    assert_eq!(list(&v["deleted"]), ["c.txt"]);
    assert!(
        !text.contains("b.txt"),
        "the untouched file is not reported"
    );
}

#[tokio::test]
async fn status_reports_a_staged_modification_and_a_detached_head() {
    let root = repo("detached");
    std::fs::write(root.join("b.txt"), "bee\nmore\n").unwrap();
    git(&root, &["add", "b.txt"]);
    git(&root, &["checkout", "-q", "--detach"]);
    let (_, text) = observe(&ProjectExecutor::new(&root), PROJECT_GIT_STATUS).await;
    let v = first_line(&text);
    assert_eq!(list(&v["staged"]), ["b.txt"]);
    assert!(list(&v["unstaged"]).is_empty());
    assert!(
        v["branch"].is_null(),
        "a detached HEAD has no branch: {text}"
    );
}

#[tokio::test]
async fn a_directory_that_is_not_a_repository_is_an_observation_not_an_invented_state() {
    let root = scratch("norepo");
    for capability in GIT_CAPABILITIES {
        let (ok, text) = observe(&ProjectExecutor::new(&root), capability).await;
        assert!(!ok);
        assert_eq!(
            first_line(&text)["error"],
            "not_a_repository",
            "{capability}"
        );
    }
}

#[tokio::test]
async fn a_subdirectory_of_a_repository_does_not_reach_the_repository_above_it() {
    let outer = repo("outer");
    let inner = outer.join("src");
    let (ok, text) = observe(&ProjectExecutor::new(&inner), PROJECT_GIT_STATUS).await;
    assert!(!ok);
    assert_eq!(first_line(&text)["error"], "not_a_repository", "{text}");
}

#[cfg(unix)]
#[tokio::test]
async fn a_git_dir_that_is_a_symlink_or_a_file_is_refused_not_followed() {
    let elsewhere = repo("elsewhere");
    let linked = scratch("linked");
    std::os::unix::fs::symlink(elsewhere.join(".git"), linked.join(".git")).unwrap();
    let (ok, text) = observe(&ProjectExecutor::new(&linked), PROJECT_GIT_STATUS).await;
    assert!(!ok);
    assert_eq!(first_line(&text)["error"], "unsafe_git_dir", "{text}");
    assert!(
        !text.contains("a.txt"),
        "nothing of the other repository is shown"
    );

    let filed = scratch("gitfile");
    std::fs::write(
        filed.join(".git"),
        format!("gitdir: {}\n", elsewhere.join(".git").display()),
    )
    .unwrap();
    let (ok, text) = observe(&ProjectExecutor::new(&filed), PROJECT_GIT_STATUS).await;
    assert!(!ok);
    assert_eq!(first_line(&text)["error"], "unsafe_git_dir", "{text}");
}

#[tokio::test]
async fn a_repository_that_git_cannot_read_is_a_git_failure_not_a_guess() {
    let root = scratch("broken");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(root.join(".git/config"), "nothing useful\n").unwrap();
    let (ok, text) = observe(&ProjectExecutor::new(&root), PROJECT_GIT_STATUS).await;
    assert!(!ok);
    assert_eq!(first_line(&text)["error"], "git_failed", "{text}");
}

// ---- diff and diff_stat -------------------------------------------------------------------------------

#[tokio::test]
async fn diff_is_the_actual_working_tree_diff_split_into_unstaged_and_staged() {
    let root = repo("diff");
    make_dirty(&root);
    std::fs::write(root.join("b.txt"), "bee\nbuzz\n").unwrap();
    git(&root, &["add", "b.txt"]);
    let (ok, text) = observe(&ProjectExecutor::new(&root), PROJECT_GIT_DIFF).await;
    assert!(ok, "{text}");
    let v = first_line(&text);
    assert_eq!(
        (v["complete"].clone(), v["includes_untracked"].clone()),
        (true.into(), false.into())
    );
    let (_, rest) = text.split_once("--- unstaged diff ---\n").unwrap();
    let (unstaged, staged) = rest.split_once("\n--- staged diff ---\n").unwrap();
    assert_eq!(v["unstaged_bytes"], unstaged.len());
    assert_eq!(v["staged_bytes"], staged.len());
    // The same text Git itself produces for the same state.
    let real = |extra: &[&str]| {
        let mut args = vec!["diff", "--no-color", "--no-renames"];
        args.extend(extra);
        String::from_utf8(
            Command::new("git")
                .args(args)
                .current_dir(&root)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
    };
    assert_eq!(unstaged, real(&[]));
    assert_eq!(staged, real(&["--cached"]));
    assert!(unstaged.contains("-two") && unstaged.contains("+TWO") && unstaged.contains("+four"));
    assert!(
        unstaged.contains("deleted file mode"),
        "the deleted file is in the diff"
    );
    assert!(staged.contains("+buzz") && staged.contains("new file mode"));
    assert!(
        !text.contains("untracked.txt"),
        "an untracked file is not in a diff"
    );
}

#[tokio::test]
async fn a_clean_tree_has_an_empty_diff_and_an_empty_stat() {
    let root = repo("nodiff");
    let p = ProjectExecutor::new(&root);
    let (ok, text) = observe(&p, PROJECT_GIT_DIFF).await;
    assert!(ok);
    assert_eq!(first_line(&text)["unstaged_bytes"], 0);
    let (ok, text) = observe(&p, PROJECT_GIT_DIFF_STAT).await;
    assert!(ok);
    let v = first_line(&text);
    assert_eq!(v["unstaged"]["files_changed"], 0);
    assert_eq!(v["staged"]["files_changed"], 0);
}

#[tokio::test]
async fn a_diff_over_the_bound_fails_rather_than_truncate() {
    let root = repo("bigdiff");
    let big: String = (0..4000)
        .map(|i| format!("line {i} of a large change\n"))
        .collect();
    assert!(big.len() > MAX_GIT_DIFF_BYTES);
    std::fs::write(root.join("a.txt"), &big).unwrap();
    let p = ProjectExecutor::new(&root);
    let (ok, text) = observe(&p, PROJECT_GIT_DIFF).await;
    assert!(!ok, "a too-large diff is not a successful observation");
    let v = first_line(&text);
    assert_eq!(v["error"], "too_large");
    assert!(
        v.get("complete").is_none() && text.lines().count() == 1,
        "no partial diff: {text}"
    );
    // The summary is still available, and it is exact.
    let (ok, stat) = observe(&p, PROJECT_GIT_DIFF_STAT).await;
    assert!(ok, "{stat}");
    assert_eq!(first_line(&stat)["unstaged"]["insertions"], 4000);
    assert_eq!(first_line(&stat)["unstaged"]["deletions"], 3);
}

#[tokio::test]
async fn the_bound_covers_both_sides_together() {
    let root = repo("bothsides");
    let half: String = (0..1000).map(|i| format!("a line number {i}\n")).collect();
    std::fs::write(root.join("a.txt"), &half).unwrap();
    git(&root, &["add", "a.txt"]);
    let more: String = half.lines().map(|l| format!("{l} edited\n")).collect();
    std::fs::write(root.join("a.txt"), &more).unwrap();
    let (ok, text) = observe(&ProjectExecutor::new(&root), PROJECT_GIT_DIFF).await;
    assert!(
        !ok,
        "each side fits alone, together they do not: {} bytes",
        text.len()
    );
    assert_eq!(first_line(&text)["error"], "too_large");
}

#[tokio::test]
async fn diff_stat_counts_match_the_real_repository() {
    let root = repo("stat");
    make_dirty(&root);
    std::fs::write(root.join("b.txt"), "bee\nbuzz\nbuzz\n").unwrap();
    git(&root, &["add", "b.txt"]);
    let (ok, text) = observe(&ProjectExecutor::new(&root), PROJECT_GIT_DIFF_STAT).await;
    assert!(ok, "{text}");
    let v = first_line(&text);
    // Unstaged: a.txt (+2 -1), c.txt (deleted, -1). Staged: b.txt (+2), staged.txt (+1).
    let u = &v["unstaged"];
    assert_eq!(
        (
            u["files_changed"].clone(),
            u["insertions"].clone(),
            u["deletions"].clone()
        ),
        (2.into(), 2.into(), 2.into())
    );
    let s = &v["staged"];
    assert_eq!(
        (
            s["files_changed"].clone(),
            s["insertions"].clone(),
            s["deletions"].clone()
        ),
        (2.into(), 3.into(), 0.into())
    );
    let real = String::from_utf8(
        Command::new("git")
            .args(["diff", "--numstat"])
            .current_dir(&root)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert_eq!(real.lines().count(), 2, "{real}");
    assert_eq!(u["files"][0]["path"], "a.txt");
    assert_eq!(u["files"][0]["insertions"], 2);
}

#[tokio::test]
async fn a_binary_change_is_counted_as_a_file_without_line_counts() {
    let root = repo("binary");
    std::fs::write(root.join("blob.bin"), [0u8, 1, 2, 3, 0, 255]).unwrap();
    git(&root, &["add", "blob.bin"]);
    git(&root, &["commit", "-q", "-m", "blob"]);
    std::fs::write(root.join("blob.bin"), [0u8, 9, 9, 9, 0, 254, 1]).unwrap();
    let p = ProjectExecutor::new(&root);
    let (ok, text) = observe(&p, PROJECT_GIT_DIFF_STAT).await;
    assert!(ok, "{text}");
    let f = &first_line(&text)["unstaged"]["files"][0];
    assert_eq!(
        (f["path"].clone(), f["binary"].clone()),
        ("blob.bin".into(), true.into())
    );
    assert!(f["insertions"].is_null());
    let (ok, diff) = observe(&p, PROJECT_GIT_DIFF).await;
    assert!(ok, "{diff}");
}

// ---- log ----------------------------------------------------------------------------------------------

#[tokio::test]
async fn log_is_bounded_newest_first_and_deterministic() {
    let root = repo("log");
    let p = ProjectExecutor::new(&root);
    let ask = |n: i64| inputs(&[("count", InputValue::Integer(n))]);
    let r = run(&p, PROJECT_GIT_LOG, ask(2)).await.unwrap();
    let text = r.output;
    let v = first_line(&text);
    assert_eq!(
        (v["requested"].clone(), v["returned"].clone()),
        (2.into(), 2.into())
    );
    let subjects: Vec<&str> = v["commits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["subject"].as_str().unwrap())
        .collect();
    assert_eq!(subjects, ["third", "second"]);
    let head = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&root)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert_eq!(v["commits"][0]["hash"], head.trim());
    let again = run(&p, PROJECT_GIT_LOG, ask(2)).await.unwrap().output;
    assert_eq!(text, again, "deterministic");
    // Default count, and a count larger than the history.
    let all = run(&p, PROJECT_GIT_LOG, BTreeMap::new())
        .await
        .unwrap()
        .output;
    assert_eq!(first_line(&all)["requested"], DEFAULT_GIT_LOG_COUNT);
    assert_eq!(first_line(&all)["returned"], 3);
    let big = run(&p, PROJECT_GIT_LOG, ask(MAX_GIT_LOG_COUNT))
        .await
        .unwrap()
        .output;
    assert_eq!(first_line(&big)["returned"], 3);
}

#[tokio::test]
async fn log_enforces_its_maximum_on_a_longer_history() {
    let root = repo("longlog");
    for i in 0..(MAX_GIT_LOG_COUNT + 5) {
        std::fs::write(root.join("a.txt"), format!("{i}\n")).unwrap();
        git(&root, &["commit", "-q", "-am", &format!("change {i}")]);
    }
    let p = ProjectExecutor::new(&root);
    let r = run(
        &p,
        PROJECT_GIT_LOG,
        inputs(&[("count", InputValue::Integer(MAX_GIT_LOG_COUNT))]),
    )
    .await
    .unwrap();
    assert_eq!(first_line(&r.output)["returned"], MAX_GIT_LOG_COUNT);
    let over = run(
        &p,
        PROJECT_GIT_LOG,
        inputs(&[("count", InputValue::Integer(MAX_GIT_LOG_COUNT + 1))]),
    )
    .await;
    assert!(matches!(over, Err(ExecutionError::InvalidRequest(_))));
    let r = run(&p, PROJECT_GIT_LOG, BTreeMap::new()).await.unwrap();
    assert_eq!(first_line(&r.output)["returned"], DEFAULT_GIT_LOG_COUNT);
}

#[tokio::test]
async fn the_log_of_a_repository_with_no_commit_is_empty() {
    let root = scratch("unborn");
    git(&root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("new.txt"), "x\n").unwrap();
    let p = ProjectExecutor::new(&root);
    let (ok, text) = observe(&p, PROJECT_GIT_LOG).await;
    assert!(ok, "{text}");
    assert_eq!(first_line(&text)["returned"], 0);
    let (ok, text) = observe(&p, PROJECT_GIT_STATUS).await;
    assert!(ok, "{text}");
    assert_eq!(list(&first_line(&text)["untracked"]), ["new.txt"]);
    let (ok, _) = observe(&p, PROJECT_GIT_DIFF).await;
    assert!(ok);
}

// ---- invalid invocation -------------------------------------------------------------------------------

#[tokio::test]
async fn every_invalid_git_invocation_is_refused_before_anything_runs() {
    let root = repo("invalid");
    let p = ProjectExecutor::new(&root);
    let text = |s: &str| InputValue::Text(s.to_string());
    let no_input_cases: Vec<(&str, BTreeMap<String, InputValue>)> = vec![
        ("empty inputs object content", inputs(&[("x", text("y"))])),
        ("a path", inputs(&[("path", text("src"))])),
        (
            "a repository path",
            inputs(&[("repository", text("/other/repo"))]),
        ),
        ("a repo", inputs(&[("repo", text("../../repo"))])),
        ("a cwd", inputs(&[("cwd", text("/"))])),
        ("a git dir", inputs(&[("git_dir", text("/etc"))])),
        (
            "a command",
            inputs(&[("command", text("git commit -am x"))]),
        ),
        ("an executable", inputs(&[("executable", text("/bin/sh"))])),
        ("an argv", inputs(&[("argv", text("--exec=sh"))])),
        ("a revision", inputs(&[("revision", text("HEAD~1"))])),
        ("a flag", inputs(&[("flags", text("--output=/tmp/x"))])),
        ("a count", inputs(&[("count", InputValue::Integer(3))])),
    ];
    for capability in [PROJECT_GIT_STATUS, PROJECT_GIT_DIFF, PROJECT_GIT_DIFF_STAT] {
        for (what, given) in &no_input_cases {
            let r = run(&p, capability, given.clone()).await;
            assert!(
                matches!(r, Err(ExecutionError::InvalidRequest(_))),
                "{capability} {what}"
            );
        }
    }
    let log_cases: Vec<(&str, BTreeMap<String, InputValue>)> = vec![
        ("count zero", inputs(&[("count", InputValue::Integer(0))])),
        (
            "count negative",
            inputs(&[("count", InputValue::Integer(-1))]),
        ),
        (
            "count over the maximum",
            inputs(&[("count", InputValue::Integer(MAX_GIT_LOG_COUNT + 1))]),
        ),
        (
            "count i64 max",
            inputs(&[("count", InputValue::Integer(i64::MAX))]),
        ),
        ("count text", inputs(&[("count", text("5"))])),
        ("count flag text", inputs(&[("count", text("5 --all"))])),
        ("count bool", inputs(&[("count", InputValue::Bool(true))])),
        (
            "an undeclared input",
            inputs(&[
                ("count", InputValue::Integer(2)),
                ("revision", text("main")),
            ]),
        ),
        ("a revision", inputs(&[("revision", text("--all"))])),
        ("a path", inputs(&[("path", text("src/lib.rs"))])),
        (
            "an executable",
            inputs(&[("executable", text("/usr/bin/env"))]),
        ),
        ("an argv", inputs(&[("argv", text("--exec"))])),
        ("a repository path", inputs(&[("repository", text("../x"))])),
    ];
    for (what, given) in log_cases {
        let id = CapabilityId::new(PROJECT_GIT_LOG).unwrap();
        assert!(
            p.validate_inputs(&id, &given).await.is_err(),
            "validate: {what}"
        );
        let r = run(&p, PROJECT_GIT_LOG, given).await;
        assert!(
            matches!(r, Err(ExecutionError::InvalidRequest(_))),
            "execute: {what}"
        );
    }
    // And no mutation capability exists to ask for.
    for id in [
        "project.git.commit",
        "project.git.add",
        "project.git.checkout",
        "project.git.reset",
        "project.git.push",
        "project.git.clean",
        "project.git",
        "project.git.run",
        "git",
    ] {
        let r = run(&p, id, BTreeMap::new()).await;
        assert!(matches!(r, Err(ExecutionError::InvalidRequest(_))), "{id}");
    }
}

// ---- nothing changes ----------------------------------------------------------------------------------

#[tokio::test]
async fn invoking_every_git_capability_changes_nothing() {
    let root = repo("immutable");
    make_dirty(&root);
    let p = ProjectExecutor::new(&root);
    let before = snapshot(&root); // work tree and the whole .git directory, byte for byte
    let git_out = |args: &[&str]| {
        String::from_utf8(
            Command::new("git")
                .args(args)
                .current_dir(&root)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
    };
    let (head, branches, commits) = (
        git_out(&["rev-parse", "HEAD"]),
        git_out(&["branch", "--list", "-a"]),
        git_out(&["rev-list", "--all", "--count"]),
    );
    for _ in 0..2 {
        for capability in GIT_CAPABILITIES {
            let (ok, text) = observe(&p, capability).await;
            assert!(ok, "{capability}: {text}");
        }
        run(
            &p,
            PROJECT_GIT_LOG,
            inputs(&[("count", InputValue::Integer(2))]),
        )
        .await
        .unwrap();
    }
    assert_eq!(
        snapshot(&root),
        before,
        "files, index, HEAD, refs and objects are byte-identical"
    );
    assert_eq!(git_out(&["rev-parse", "HEAD"]), head);
    assert_eq!(git_out(&["branch", "--list", "-a"]), branches);
    assert_eq!(git_out(&["rev-list", "--all", "--count"]), commits);
    assert!(!root.join(".git/index.lock").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn a_repository_configured_to_run_programs_is_not_made_to_run_them() {
    use std::os::unix::fs::PermissionsExt;
    let root = repo("hostile-config");
    let marker = root
        .join("..")
        .join(format!("marker-{}", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let script = scratch("hostile-script").join("run.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    git(
        &root,
        &["config", "core.fsmonitor", script.to_str().unwrap()],
    );
    git(
        &root,
        &["config", "diff.external", script.to_str().unwrap()],
    );
    git(&root, &["config", "core.pager", script.to_str().unwrap()]);
    std::fs::write(root.join("a.txt"), "changed\n").unwrap();
    let p = ProjectExecutor::new(&root);
    for capability in GIT_CAPABILITIES {
        let (ok, text) = observe(&p, capability).await;
        assert!(ok, "{capability}: {text}");
    }
    assert!(!marker.exists(), "a configured program ran");
}

// ---- the audit's independent checks -------------------------------------------------------------------

#[tokio::test]
async fn the_audit_accepts_real_observations_and_rejects_forged_ones() {
    let root = repo("audit");
    make_dirty(&root);
    let p = ProjectExecutor::new(&root);
    let scope = git_scope_invariant();
    let valid = git_observation_invariant(&root);
    assert_eq!(
        (scope.name(), valid.name()),
        (GIT_SCOPE, GIT_OBSERVATION_INVALID)
    );
    for capability in GIT_CAPABILITIES {
        let (_, text) = observe(&p, capability).await;
        let kind = ObservationKind::ExecutionCompleted;
        let o = observation(kind, &text);
        assert_eq!(
            (scope.violations(&o), valid.violations(&o)),
            (0, 0),
            "{capability}"
        );
    }
    let (_, status) = observe(&p, PROJECT_GIT_STATUS).await;
    let (_, log) = observe(&p, PROJECT_GIT_LOG).await;
    let (_, diff) = observe(&p, PROJECT_GIT_DIFF).await;
    let completed = ObservationKind::ExecutionCompleted;

    // A repository other than the project's, or a path outside the work tree.
    let forged = status.replace("\"repository\":\".\"", "\"repository\":\"/other/repo\"");
    assert_eq!(scope.violations(&observation(completed, &forged)), 1);
    for bad in ["../secret", "/etc/passwd", ".git/config", "a/../../b"] {
        let forged = status.replace("untracked.txt", bad);
        assert!(
            scope.violations(&observation(completed, &forged)) > 0,
            "{bad}"
        );
    }
    // "Clean" while paths are listed; a fabricated commit; a diff that claims a different size;
    // a diff presented as complete when it is not the whole text.
    let forged = status.replace("\"clean\":false", "\"clean\":true");
    assert_eq!(valid.violations(&observation(completed, &forged)), 1);
    let hash = first_line(&log)["commits"][0]["hash"]
        .as_str()
        .unwrap()
        .to_string();
    let forged = log.replace(
        &hash,
        &"0123456789abcdef"
            .repeat(2_usize)
            .chars()
            .chain("01234567".chars())
            .collect::<String>(),
    );
    assert_eq!(valid.violations(&observation(completed, &forged)), 1);
    let forged = diff.replace("\"unstaged_bytes\":", "\"unstaged_bytes\":9");
    assert_eq!(valid.violations(&observation(completed, &forged)), 1);
    let truncated = &diff[..diff.len() - 20];
    assert_eq!(valid.violations(&observation(completed, truncated)), 1);
    // A "failure" that carries more than Chip's own one line.
    let forged = "{\"capability\":\"project.git.status\",\"error\":\"x\"}\nclean: true";
    assert_eq!(
        valid.violations(&observation(ObservationKind::ExecutionFailed, forged)),
        1
    );
    // Observations that are not Git's are not this invariant's business.
    assert_eq!(valid.violations(&observation(completed, "tests passed")), 0);
}

#[tokio::test]
async fn a_host_path_in_a_git_observation_is_a_leak() {
    let root = repo("leak");
    let leak = host_path_leak_invariant(&root);
    let (_, text) = observe(&ProjectExecutor::new(&root), PROJECT_GIT_STATUS).await;
    let completed = ObservationKind::ExecutionCompleted;
    assert_eq!(leak.violations(&observation(completed, &text)), 0);
    let forged = text.replace(
        "\"branch\":\"main\"",
        &format!("\"branch\":\"{}\"", root.display()),
    );
    assert_eq!(
        leak.violations(&observation(completed, &forged)),
        1,
        "{}",
        HOST_PATH_LEAK
    );
}
