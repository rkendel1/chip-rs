//! Read-only Git observation: `project.git.status`, `project.git.diff`, `project.git.diff_stat`
//! and `project.git.log`.
//!
//! Git is observation here, not authority. The model names one of four capabilities (and, for the
//! log, a bounded count). Chip owns everything else: the executable, every argument, the
//! repository (always the project root's own `.git`, pinned with `GIT_DIR`/`GIT_WORK_TREE`), the
//! environment, the time limit and the output bound. The model never supplies an argv, a revision,
//! a path, a flag or a working directory, and there is no generic "git" capability.
//!
//! Every argv is built by [`argv`] from a fixed allow-list of read-only subcommands. Nothing
//! mutating is expressible: no `add`, `commit`, `checkout`, `reset`, `clean`, `stash`, and so on.
//! Optional locks are disabled, so even the index is not refreshed by an observation.
//!
//! Failure is observed, never repaired: a project that is not a repository is the observation
//! `not_a_repository`; a missing `git` is `git_unavailable`; output past a bound is `too_large`
//! (a truncated diff is never presented as a complete one). There is no filesystem fallback and no
//! parsing of `.git` internals.
//!
//! Git observations are evidence about repository state. They do not establish that a coding goal
//! is satisfied.
//!
//! Known limit, stated plainly: Git itself runs repository-configured clean filters while
//! comparing content. The model cannot write `.git/config` (the path rules reserve `.git`), but a
//! repository whose owner configured a filter will run it, as it would for any `git status`.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use chip_core::{Observation, ObservationInvariant, ObservationKind};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

pub const PROJECT_GIT_STATUS: &str = "project.git.status";
pub const PROJECT_GIT_DIFF: &str = "project.git.diff";
pub const PROJECT_GIT_DIFF_STAT: &str = "project.git.diff_stat";
pub const PROJECT_GIT_LOG: &str = "project.git.log";

/// `project.git.log`: the count used when none is given, and the most that may be asked for.
pub const DEFAULT_GIT_LOG_COUNT: i64 = 10;
pub const MAX_GIT_LOG_COUNT: i64 = 50;
/// `project.git.diff`: the most diff text one observation carries (both sections together).
pub const MAX_GIT_DIFF_BYTES: usize = 32 * 1024;
/// `project.git.status` and `project.git.diff_stat`: the most paths one observation lists.
pub const MAX_GIT_PATHS: usize = 200;
/// The most raw output Chip will read from any one Git invocation for a structured observation.
const MAX_GIT_RAW_BYTES: usize = 64 * 1024;
/// The most a log observation carries.
const MAX_GIT_LOG_BYTES: usize = 16 * 1024;
const GIT_TIMEOUT: Duration = Duration::from_secs(30);

/// Invariant names, as the audit reports them.
pub const GIT_SCOPE: &str = "git_scope";
pub const GIT_OBSERVATION_INVALID: &str = "git_observation_invalid";

/// Every Git capability id.
pub const GIT_CAPABILITIES: [&str; 4] = [
    PROJECT_GIT_STATUS,
    PROJECT_GIT_DIFF,
    PROJECT_GIT_DIFF_STAT,
    PROJECT_GIT_LOG,
];

/// The only Git subcommands Chip ever runs. All are read-only.
pub const ALLOWED_SUBCOMMANDS: [&str; 4] = ["status", "diff", "log", "rev-parse"];

pub fn is_git_capability(id: &str) -> bool {
    GIT_CAPABILITIES.contains(&id)
}

/// Which side of the index a diff reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Unstaged,
    Staged,
}

/// Which Git invocation Chip is building.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Invocation {
    Status,
    Diff(Side),
    Numstat(Side),
    HasHead,
    Log { count: i64 },
}

/// The complete argument vector for an invocation. Chip's constants plus, for the log, a number
/// Chip validated and formatted itself. Nothing here comes from the model as text.
pub fn argv(invocation: Invocation) -> Vec<String> {
    let mut args: Vec<String> = [
        "--no-pager",
        "--no-replace-objects",
        "-c",
        "core.fsmonitor=false",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let mut add = |more: &[&str]| args.extend(more.iter().map(|s| s.to_string()));
    let diff_flags = [
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--no-renames",
        "--src-prefix=a/",
        "--dst-prefix=b/",
    ];
    match invocation {
        Invocation::Status => add(&[
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--untracked-files=all",
            "--no-renames",
        ]),
        Invocation::Diff(side) | Invocation::Numstat(side) => {
            add(&["diff"]);
            add(&diff_flags);
            if matches!(invocation, Invocation::Numstat(_)) {
                add(&["--numstat", "-z"]);
            }
            if side == Side::Staged {
                add(&["--cached"]);
            }
            add(&["--"]);
        }
        Invocation::HasHead => add(&["rev-parse", "--verify", "--quiet", "HEAD"]),
        Invocation::Log { count } => {
            add(&[
                "log",
                "-z",
                "--no-color",
                "--no-decorate",
                "--no-show-signature",
                "--format=%H%x1f%s",
            ]);
            args.push("-n".into());
            args.push(count.to_string());
            args.push("HEAD".into());
            args.push("--".into());
        }
    }
    args
}

enum Run {
    Output(Vec<u8>),
    /// Git ran and exited with this code (or was killed: `-1`).
    Exit(i32),
    TooLarge,
    TimedOut,
    Unavailable,
}

/// Runs `git` with Chip's argv, against the project root only, reading at most `limit` bytes.
async fn run(root: &Path, invocation: Invocation, limit: usize) -> Run {
    let null_config = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let mut command = Command::new("git");
    command
        .args(argv(invocation))
        .current_dir(root)
        .env_clear()
        .env("GIT_DIR", root.join(".git"))
        .env("GIT_WORK_TREE", root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", null_config)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    let Ok(mut child) = command.spawn() else {
        return Run::Unavailable;
    };
    let Some(mut stdout) = child.stdout.take() else {
        return Run::Unavailable;
    };
    let work = async {
        let mut bytes = Vec::new();
        if (&mut stdout)
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .await
            .is_err()
        {
            return Run::Exit(-1);
        }
        if bytes.len() > limit {
            let _ = child.kill().await;
            return Run::TooLarge;
        }
        match child.wait().await {
            Ok(status) if status.success() => Run::Output(bytes),
            Ok(status) => Run::Exit(status.code().unwrap_or(-1)),
            Err(_) => Run::Exit(-1),
        }
    };
    tokio::time::timeout(GIT_TIMEOUT, work)
        .await
        .unwrap_or(Run::TimedOut)
}

fn line(fields: Value) -> String {
    serde_json::to_string(&fields).expect("a JSON value serialises")
}

fn failure(capability: &str, error: &str) -> (bool, String) {
    (
        false,
        line(json!({"capability": capability, "error": error})),
    )
}

/// A failed run, as the observation it is. `None` for output that is usable.
fn output(capability: &str, run: Run) -> Result<Vec<u8>, (bool, String)> {
    match run {
        Run::Output(bytes) => Ok(bytes),
        Run::Exit(code) => Err((
            false,
            line(json!({"capability": capability, "error": "git_failed", "exit_code": code})),
        )),
        Run::TooLarge => Err((
            false,
            line(json!({"capability": capability, "error": "too_large"})),
        )),
        Run::TimedOut => Err(failure(capability, "timed_out")),
        Run::Unavailable => Err(failure(capability, "git_unavailable")),
    }
}

fn utf8(capability: &str, bytes: Vec<u8>) -> Result<String, (bool, String)> {
    String::from_utf8(bytes).map_err(|_| failure(capability, "not_utf8"))
}

/// Performs one Git observation. Returns `(succeeded, observation text)`.
pub(crate) async fn observe(root: &Path, capability: &str, count: Option<i64>) -> (bool, String) {
    // The repository is the project root's own `.git` directory: absent is "not a repository",
    // and a symlink or a `.git` file (which can point anywhere) is refused, not followed.
    match std::fs::symlink_metadata(root.join(".git")) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return failure(capability, "not_a_repository");
        }
        Err(_) => return failure(capability, "io_error"),
        Ok(m) if m.file_type().is_symlink() || !m.is_dir() => {
            return failure(capability, "unsafe_git_dir");
        }
        Ok(_) => {}
    }
    match capability {
        PROJECT_GIT_STATUS => status(root).await,
        PROJECT_GIT_DIFF => diff(root).await,
        PROJECT_GIT_DIFF_STAT => diff_stat(root).await,
        PROJECT_GIT_LOG => log(root, count.unwrap_or(DEFAULT_GIT_LOG_COUNT)).await,
        _ => failure(capability, "not_a_git_capability"),
    }
}

// ---- status -------------------------------------------------------------------------------------------

async fn status(root: &Path) -> (bool, String) {
    let cap = PROJECT_GIT_STATUS;
    let raw = match output(cap, run(root, Invocation::Status, MAX_GIT_RAW_BYTES).await) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let text = match utf8(cap, raw) {
        Ok(t) => t,
        Err(e) => return e,
    };
    let (mut branch, mut staged, mut unstaged, mut untracked, mut deleted, mut unmerged) = (
        None,
        BTreeSet::new(),
        BTreeSet::new(),
        BTreeSet::new(),
        BTreeSet::new(),
        BTreeSet::new(),
    );
    for record in text.split('\0').filter(|r| !r.is_empty()) {
        if let Some(rest) = record.strip_prefix("# ") {
            if let Some(head) = rest.strip_prefix("branch.head ") {
                branch = (head != "(detached)").then(|| head.to_string());
            }
            continue;
        }
        let kind = record.split(' ').next().unwrap_or_default();
        match kind {
            // `1 XY sub mH mI mW hH hI path`
            "1" => {
                let mut f = record.splitn(9, ' ');
                let (xy, path) = (f.nth(1), f.nth(6));
                let (Some(xy), Some(path)) = (xy, path) else {
                    return failure(cap, "unexpected_status_record");
                };
                let mut c = xy.chars();
                let (x, y) = (c.next().unwrap_or('.'), c.next().unwrap_or('.'));
                if x != '.' {
                    staged.insert(path.to_string());
                }
                if y != '.' {
                    unstaged.insert(path.to_string());
                }
                if x == 'D' || y == 'D' {
                    deleted.insert(path.to_string());
                }
            }
            // `u XY sub m1 m2 m3 mW h1 h2 h3 path`
            "u" => match record.splitn(11, ' ').nth(10) {
                Some(path) => {
                    unmerged.insert(path.to_string());
                }
                None => return failure(cap, "unexpected_status_record"),
            },
            "?" => match record.strip_prefix("? ") {
                Some(path) => {
                    untracked.insert(path.to_string());
                }
                None => return failure(cap, "unexpected_status_record"),
            },
            // Renames are disabled; anything else is not a record Chip knows how to read.
            _ => return failure(cap, "unexpected_status_record"),
        }
    }
    let total = staged.len() + unstaged.len() + untracked.len() + unmerged.len() + deleted.len();
    if total > MAX_GIT_PATHS {
        return failure(cap, "too_large");
    }
    let clean = staged.is_empty()
        && unstaged.is_empty()
        && untracked.is_empty()
        && deleted.is_empty()
        && unmerged.is_empty();
    (
        true,
        line(json!({
            "capability": cap,
            "repository": ".",
            "branch": branch,
            "clean": clean,
            "staged": staged,
            "unstaged": unstaged,
            "untracked": untracked,
            "deleted": deleted,
            "unmerged": unmerged,
        })),
    )
}

// ---- diff ---------------------------------------------------------------------------------------------

const UNSTAGED_MARK: &str = "\n--- unstaged diff ---\n";
const STAGED_MARK: &str = "\n--- staged diff ---\n";

async fn diff(root: &Path) -> (bool, String) {
    let cap = PROJECT_GIT_DIFF;
    let unstaged = match output(
        cap,
        run(root, Invocation::Diff(Side::Unstaged), MAX_GIT_DIFF_BYTES).await,
    ) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let remaining = MAX_GIT_DIFF_BYTES - unstaged.len();
    let staged = match output(
        cap,
        run(root, Invocation::Diff(Side::Staged), remaining).await,
    ) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let (unstaged, staged) = match (utf8(cap, unstaged), utf8(cap, staged)) {
        (Ok(u), Ok(s)) => (u, s),
        (Err(e), _) | (_, Err(e)) => return e,
    };
    // The whole diff, both sides, or a failure: never a prefix presented as the diff.
    let head = line(json!({
        "capability": cap,
        "complete": true,
        "includes_untracked": false,
        "unstaged_bytes": unstaged.len(),
        "staged_bytes": staged.len(),
    }));
    (
        true,
        format!("{head}{UNSTAGED_MARK}{unstaged}{STAGED_MARK}{staged}"),
    )
}

// ---- diff_stat ----------------------------------------------------------------------------------------

async fn numstat(root: &Path, side: Side) -> Result<Value, (bool, String)> {
    let cap = PROJECT_GIT_DIFF_STAT;
    let raw = output(
        cap,
        run(root, Invocation::Numstat(side), MAX_GIT_RAW_BYTES).await,
    )?;
    let text = utf8(cap, raw)?;
    let (mut files, mut insertions, mut deletions) = (Vec::new(), 0u64, 0u64);
    for record in text.split('\0').filter(|r| !r.is_empty()) {
        let mut f = record.splitn(3, '\t');
        let (Some(ins), Some(del), Some(path)) = (f.next(), f.next(), f.next()) else {
            return Err(failure(cap, "unexpected_numstat_record"));
        };
        // A binary file has no line counts.
        let counts = match (ins.parse::<u64>(), del.parse::<u64>()) {
            (Ok(i), Ok(d)) => Some((i, d)),
            _ if ins == "-" && del == "-" => None,
            _ => return Err(failure(cap, "unexpected_numstat_record")),
        };
        if let Some((i, d)) = counts {
            insertions += i;
            deletions += d;
        }
        files.push(json!({
            "path": path,
            "binary": counts.is_none(),
            "insertions": counts.map(|c| c.0),
            "deletions": counts.map(|c| c.1),
        }));
    }
    if files.len() > MAX_GIT_PATHS {
        return Err(failure(cap, "too_large"));
    }
    Ok(json!({
        "files_changed": files.len(),
        "insertions": insertions,
        "deletions": deletions,
        "files": files,
    }))
}

async fn diff_stat(root: &Path) -> (bool, String) {
    let unstaged = match numstat(root, Side::Unstaged).await {
        Ok(v) => v,
        Err(e) => return e,
    };
    let staged = match numstat(root, Side::Staged).await {
        Ok(v) => v,
        Err(e) => return e,
    };
    (
        true,
        line(json!({
            "capability": PROJECT_GIT_DIFF_STAT,
            "includes_untracked": false,
            "unstaged": unstaged,
            "staged": staged,
        })),
    )
}

// ---- log ----------------------------------------------------------------------------------------------

async fn log(root: &Path, count: i64) -> (bool, String) {
    let cap = PROJECT_GIT_LOG;
    if !(1..=MAX_GIT_LOG_COUNT).contains(&count) {
        // Unreachable through validation; refused again where the argument is built.
        return failure(cap, "count_out_of_range");
    }
    // A repository with no commit has an empty history, which is an observation, not an error.
    match run(root, Invocation::HasHead, 64).await {
        Run::Output(_) => {}
        Run::Exit(1) => {
            return (
                true,
                line(json!({"capability": cap, "requested": count, "returned": 0, "commits": []})),
            );
        }
        other => {
            return output(cap, other)
                .err()
                .unwrap_or_else(|| failure(cap, "git_failed"));
        }
    }
    let raw = match output(
        cap,
        run(root, Invocation::Log { count }, MAX_GIT_LOG_BYTES).await,
    ) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let text = match utf8(cap, raw) {
        Ok(t) => t,
        Err(e) => return e,
    };
    let mut commits = Vec::new();
    for record in text.split('\0').filter(|r| !r.is_empty()) {
        let Some((hash, subject)) = record.trim_start_matches('\n').split_once('\u{1f}') else {
            return failure(cap, "unexpected_log_record");
        };
        commits.push(json!({"hash": hash, "subject": subject}));
    }
    (
        true,
        line(json!({
            "capability": cap,
            "requested": count,
            "returned": commits.len(),
            "commits": commits,
        })),
    )
}

// ---- audit --------------------------------------------------------------------------------------------

/// The canonical first line of a Git observation, parsed.
fn git_line(observation: &Observation) -> Option<Value> {
    let first = observation.output.as_deref()?.lines().next()?;
    let value: Value = serde_json::from_str(first).ok()?;
    is_git_capability(value.get("capability")?.as_str()?).then_some(value)
}

pub(crate) fn is_git_observation(observation: &Observation) -> bool {
    git_line(observation).is_some()
}

fn strings(v: &Value) -> Option<Vec<&str>> {
    v.as_array()?.iter().map(Value::as_str).collect()
}

/// The paths a Git observation names, from its structured first line.
fn named_paths(v: &Value) -> Vec<String> {
    let mut paths = Vec::new();
    for key in ["staged", "unstaged", "untracked", "deleted", "unmerged"] {
        if let Some(list) = strings(&v[key]) {
            paths.extend(list.into_iter().map(str::to_string));
        }
    }
    for side in ["unstaged", "staged"] {
        if let Some(files) = v[side]["files"].as_array() {
            paths.extend(
                files
                    .iter()
                    .filter_map(|f| f["path"].as_str().map(str::to_string)),
            );
        }
    }
    paths
}

/// A path as Git reports it, read with the audit's own rules: project-relative, inside, and not
/// inside `.git`.
fn inside_worktree(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.contains('\0')
        && path
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != ".." && c != ".git")
}

#[derive(Debug)]
struct GitScope;

impl ObservationInvariant for GitScope {
    fn name(&self) -> &'static str {
        GIT_SCOPE
    }

    /// A Git observation that names a repository other than the project's own, or a path that is
    /// not a project-relative path inside the work tree.
    fn violations(&self, observation: &Observation) -> usize {
        let Some(v) = git_line(observation) else {
            return 0;
        };
        let wrong_repository = v.get("repository").is_some_and(|r| r != ".");
        usize::from(wrong_repository)
            + named_paths(&v)
                .iter()
                .filter(|p| !inside_worktree(p))
                .count()
    }
}

/// Git observations are internally consistent, within their bounds, and (for a log) name real
/// commits of the project's repository, read again by the audit itself. A fabricated or
/// inconsistent Git observation is a violation.
#[derive(Debug)]
struct GitObservationInvalid {
    root: std::path::PathBuf,
}

impl GitObservationInvalid {
    /// `hash` names a commit object in the project's repository, checked by an independent
    /// read-only `git cat-file`.
    fn real_commit(&self, hash: &str) -> bool {
        hash.len() == 40
            && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            && std::process::Command::new("git")
                .args(["cat-file", "-e", &format!("{hash}^{{commit}}")])
                .env_clear()
                .env("GIT_DIR", self.root.join(".git"))
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
    }

    fn consistent(&self, v: &Value, text: &str) -> bool {
        let capability = v["capability"].as_str().unwrap_or_default();
        match capability {
            PROJECT_GIT_STATUS => {
                let lists: Option<Vec<Vec<&str>>> =
                    ["staged", "unstaged", "untracked", "deleted", "unmerged"]
                        .iter()
                        .map(|k| strings(&v[*k]))
                        .collect();
                let (Some(lists), Some(clean)) = (lists, v["clean"].as_bool()) else {
                    return false;
                };
                let total: usize = lists.iter().map(Vec::len).sum();
                let touched: BTreeSet<&str> = lists[0].iter().chain(&lists[1]).copied().collect();
                clean == (total == 0)
                    && total <= MAX_GIT_PATHS
                    && lists[3].iter().all(|p| touched.contains(p))
                    && lists[2].iter().all(|p| !touched.contains(p))
                    && (v["branch"].is_null() || v["branch"].is_string())
            }
            PROJECT_GIT_DIFF => {
                let (Some(u), Some(s)) = (
                    v["unstaged_bytes"].as_u64().map(|n| n as usize),
                    v["staged_bytes"].as_u64().map(|n| n as usize),
                ) else {
                    return false;
                };
                let head_len = text.lines().next().map_or(0, str::len);
                v["complete"] == true
                    && u + s <= MAX_GIT_DIFF_BYTES
                    && text.len() == head_len + UNSTAGED_MARK.len() + u + STAGED_MARK.len() + s
                    && text[head_len..].starts_with(UNSTAGED_MARK)
                    && text
                        .get(head_len + UNSTAGED_MARK.len() + u..)
                        .is_some_and(|rest| rest.starts_with(STAGED_MARK))
            }
            PROJECT_GIT_DIFF_STAT => ["unstaged", "staged"].iter().all(|side| {
                let part = &v[*side];
                let Some(files) = part["files"].as_array() else {
                    return false;
                };
                let sum = |k: &str| files.iter().filter_map(|f| f[k].as_u64()).sum::<u64>();
                part["files_changed"].as_u64() == Some(files.len() as u64)
                    && files.len() <= MAX_GIT_PATHS
                    && part["insertions"].as_u64() == Some(sum("insertions"))
                    && part["deletions"].as_u64() == Some(sum("deletions"))
            }),
            PROJECT_GIT_LOG => {
                let Some(commits) = v["commits"].as_array() else {
                    return false;
                };
                v["returned"].as_u64() == Some(commits.len() as u64)
                    && v["requested"]
                        .as_i64()
                        .is_some_and(|n| (1..=MAX_GIT_LOG_COUNT).contains(&n))
                    && commits.len() as i64 <= v["requested"].as_i64().unwrap_or(0)
                    && text.len() <= MAX_GIT_LOG_BYTES + 1024
                    && commits.iter().all(|c| {
                        c["subject"].is_string()
                            && c["hash"].as_str().is_some_and(|h| self.real_commit(h))
                    })
            }
            _ => false,
        }
    }
}

impl ObservationInvariant for GitObservationInvalid {
    fn name(&self) -> &'static str {
        GIT_OBSERVATION_INVALID
    }

    fn violations(&self, observation: &Observation) -> usize {
        let Some(v) = git_line(observation) else {
            return 0;
        };
        let text = observation.output.as_deref().unwrap_or_default();
        let ok = match observation.kind {
            ObservationKind::ExecutionCompleted => self.consistent(&v, text),
            // A failure is only ever Chip's own `{capability, error[, exit_code]}`.
            _ => v["error"].is_string() && text.lines().count() == 1,
        };
        usize::from(!ok)
    }
}

/// Audit: Git observations name only the project's own repository and work-tree paths.
pub fn git_scope_invariant() -> std::sync::Arc<dyn ObservationInvariant> {
    std::sync::Arc::new(GitScope)
}

/// Audit: Git observations are consistent, bounded and genuine.
pub fn git_observation_invariant(
    root: impl AsRef<Path>,
) -> std::sync::Arc<dyn ObservationInvariant> {
    std::sync::Arc::new(GitObservationInvalid {
        root: root.as_ref().to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MUTATING: [&str; 16] = [
        "add", "commit", "push", "pull", "fetch", "checkout", "switch", "reset", "restore",
        "clean", "merge", "rebase", "stash", "branch", "tag", "config",
    ];

    #[test]
    fn every_argv_is_a_read_only_subcommand_chip_built() {
        let all = [
            Invocation::Status,
            Invocation::Diff(Side::Unstaged),
            Invocation::Diff(Side::Staged),
            Invocation::Numstat(Side::Unstaged),
            Invocation::Numstat(Side::Staged),
            Invocation::HasHead,
            Invocation::Log { count: 7 },
        ];
        for inv in all {
            let args = argv(inv);
            let sub = args
                .iter()
                .find(|a| ALLOWED_SUBCOMMANDS.contains(&a.as_str()))
                .unwrap_or_else(|| panic!("no allowed subcommand in {args:?}"));
            assert!(!MUTATING.contains(&sub.as_str()), "{args:?}");
            for bad in MUTATING.iter().chain(&["--exec", "--output", "-o"]) {
                assert!(!args.iter().any(|a| a == bad), "{args:?} contains {bad}");
            }
            assert!(!args.iter().any(|a| a.contains('/') && a.starts_with('/')));
        }
        assert_eq!(
            argv(Invocation::Log { count: 7 })
                .iter()
                .skip_while(|a| *a != "-n")
                .collect::<Vec<_>>(),
            ["-n", "7", "HEAD", "--"]
        );
    }

    #[test]
    fn status_records_are_read_without_guessing() {
        assert!(is_git_capability(PROJECT_GIT_LOG) && !is_git_capability("project.git.commit"));
        assert!(inside_worktree("src/lib.rs") && !inside_worktree(".git/config"));
        assert!(!inside_worktree("/etc/passwd") && !inside_worktree("../x"));
    }
}
