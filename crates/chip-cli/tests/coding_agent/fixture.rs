//! The project under test: a committed, self-contained Rust crate (`courier`) that is copied to a
//! scratch directory and put under Git for each run. The fixture is never a member of the Chip
//! workspace and has no dependencies, so it builds and tests offline.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use chip_pax::PaxExecutor;

pub const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/courier");

pub fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "e2e")
        .env("GIT_AUTHOR_EMAIL", "e2e@example.com")
        .env("GIT_COMMITTER_NAME", "e2e")
        .env("GIT_COMMITTER_EMAIL", "e2e@example.com")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn skip(rel: &str) -> bool {
    rel == "target" || rel.starts_with("target/") || rel == "Cargo.lock" || rel.starts_with(".git/")
}

/// Every file under `dir` (relative path to bytes), without build output or Git's own files.
pub fn read_tree(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            let rel = path
                .strip_prefix(base)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if skip(&rel) || rel == ".git" {
                continue;
            }
            if path.is_dir() {
                walk(base, &path, out);
            } else {
                out.insert(rel, std::fs::read(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

/// The text files of a tree.
pub fn read_text_tree(dir: &Path) -> BTreeMap<String, String> {
    read_tree(dir)
        .into_iter()
        .filter_map(|(k, v)| String::from_utf8(v).ok().map(|s| (k, s)))
        .collect()
}

/// Where scratch copies live: inside the build directory, so `cargo clean` reclaims them.
fn work_root() -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("coding-agent-work")
}

/// Removes scratch copies left by earlier runs of this test (other process ids).
pub fn sweep_stale() {
    let mine = format!("{}-", std::process::id());
    if let Ok(entries) = std::fs::read_dir(work_root()) {
        for e in entries.flatten() {
            if !e.file_name().to_string_lossy().starts_with(&mine) {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
}

/// A scratch copy of the fixture, committed to a fresh Git repository.
pub fn materialize(tag: &str) -> PathBuf {
    let dir = work_root().join(format!("{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for (rel, bytes) in read_tree(Path::new(FIXTURE)) {
        let to = dir.join(&rel);
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::write(to, bytes).unwrap();
    }
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "fixture baseline"]);
    dir
}

/// Source files and lines in the fixture, as evidence that it is a project and not a toy.
pub struct Size {
    pub rust_files: usize,
    pub rust_lines: usize,
    pub files: usize,
}

pub fn size(dir: &Path) -> Size {
    let tree = read_text_tree(dir);
    let rust: Vec<_> = tree.iter().filter(|(k, _)| k.ends_with(".rs")).collect();
    Size {
        rust_files: rust.len(),
        rust_lines: rust.iter().map(|(_, v)| v.lines().count()).sum(),
        files: tree.len(),
    }
}

/// The resolved PAX, or `None` (and a SKIPPED line) when PAX is not installed.
pub async fn pax_version(dir: &Path) -> Option<(PathBuf, String)> {
    match PaxExecutor::new(dir).resolve().await {
        Ok(p) => Some((p.path, p.version)),
        Err(e) => {
            eprintln!("SKIPPED: PAX is not usable ({e:?})");
            None
        }
    }
}
