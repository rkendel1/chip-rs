//! Where snapshots live: `.chip/graph/<snapshot-id>.json` plus a `latest` pointer.
//!
//! A stored snapshot is a cache of a projection. It is not authoritative: it can be deleted
//! at any time and rebuilt from source, and reading one verifies it against its own id.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use crate::model::ArchitectureGraph;

#[derive(Debug)]
pub enum StoreError {
    /// No snapshot has been written yet.
    NotFound,
    Io(String),
    /// A snapshot exists but is damaged or inconsistent.
    Invalid(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::NotFound => f.write_str("no architecture snapshot found"),
            StoreError::Io(m) => write!(f, "snapshot io error: {m}"),
            StoreError::Invalid(m) => write!(f, "invalid snapshot: {m}"),
        }
    }
}

impl std::error::Error for StoreError {}

fn store_dir(root: &Path) -> PathBuf {
    root.join(".chip").join("graph")
}

fn io(path: &Path, e: std::io::Error) -> StoreError {
    StoreError::Io(format!("{}: {e}", path.display()))
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, bytes).map_err(|e| io(&tmp, e))?;
    fs::rename(&tmp, path).map_err(|e| io(path, e))
}

/// Writes the snapshot under `root/.chip/graph/` and points `latest` at it. Returns the
/// path of the snapshot file. Never touches the repository's own files or `.gitignore`.
pub fn write_snapshot(root: &Path, graph: &ArchitectureGraph) -> Result<PathBuf, StoreError> {
    let dir = store_dir(root);
    fs::create_dir_all(&dir).map_err(|e| io(&dir, e))?;
    let stem = graph.snapshot_id.file_stem();
    let file = dir.join(format!("{stem}.json"));
    atomic_write(&file, graph.to_snapshot_json().as_bytes())?;
    atomic_write(&dir.join("latest"), format!("{stem}\n").as_bytes())?;
    Ok(file)
}

/// Reads the most recently written snapshot. Does not rebuild anything.
pub fn read_latest(root: &Path) -> Result<ArchitectureGraph, StoreError> {
    let dir = store_dir(root);
    let pointer = dir.join("latest");
    let stem = match fs::read_to_string(&pointer) {
        Ok(text) => text.trim().to_string(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(StoreError::NotFound),
        Err(e) => return Err(io(&pointer, e)),
    };
    if stem.is_empty() || !stem.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(StoreError::Invalid(
            "latest pointer is malformed".to_string(),
        ));
    }
    let file = dir.join(format!("{stem}.json"));
    let text = match fs::read_to_string(&file) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(StoreError::NotFound),
        Err(e) => return Err(io(&file, e)),
    };
    let graph = ArchitectureGraph::from_snapshot_json(&text).map_err(StoreError::Invalid)?;
    if graph.snapshot_id.file_stem() != stem {
        return Err(StoreError::Invalid(
            "snapshot id does not match its file name".to_string(),
        ));
    }
    Ok(graph)
}
