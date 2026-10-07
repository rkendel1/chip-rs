//! Bounded project-file capabilities: `project.read` and `project.write`.
//!
//! The model names a project-relative path (and, to write, the complete new content). Chip owns
//! everything else: the project root, path normalisation, traversal and symlink prevention, size
//! limits, the real filesystem operation, and the observation of it. The model is never given a
//! filesystem primitive: it cannot name a root, an absolute path, a descriptor, a permission, an
//! environment or a process.
//!
//! Two kinds of refusal, kept apart on purpose:
//!
//! * A request that could escape or misuse the boundary (an absolute path, `..`, a symlink on the
//!   path, a reserved name, an oversized or malformed value) is an *invalid invocation*. It is
//!   refused in [`CapabilityProvider::validate_inputs`], before anything executes, so it produces
//!   no execution, no observation and no evidence. Nothing is repaired or rewritten.
//! * A valid request that reality says no to (the file does not exist, is too large to read, is
//!   not UTF-8, the parent directory is missing, the disk refuses) is performed and *observed* as
//!   a failure. It is real, recoverable information, and no mutation took place.
//!
//! A write is atomic (a temporary file in the same directory, flushed, then renamed over the
//! target) and is reported as having succeeded only after the target has been read back and
//! found to hold exactly the requested bytes. Every observation is generated here from the real
//! operation; the first line is a canonical JSON object and, for a read, the file's content
//! follows a delimiter line.
//!
//! The observation invariants ([`path_escape_invariant`], [`out_of_root_write_invariant`]) let the
//! safety audit re-check, independently of this executor's own validation, that no recorded
//! observation names a path outside the root.
//!
//! Known limits, stated plainly: a project root that another process rewrites concurrently can
//! race the checks (the check and the operation are not one atomic step); a crash between the
//! temporary file and the rename can leave a `.chip-write-*.tmp` file behind; and writing code
//! that the project's own tooling later runs is, inherently, code execution through that tooling.
//! This crate bounds what the *model* may do to files. It does not sandbox the project.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use chip_core::{
    CapabilityAvailability, CapabilityDescriptor, CapabilityError, CapabilityId, CapabilityInput,
    CapabilityProvider, ExecutionError, ExecutionRequest, ExecutionResult, Executor, InputValue,
    Observation, ObservationInvariant, ObservationKind,
};
use sha2::{Digest, Sha256};

pub const PROJECT_READ: &str = "project.read";
pub const PROJECT_WRITE: &str = "project.write";
pub const PROJECT_LIST: &str = "project.list";
pub const PROJECT_SEARCH: &str = "project.search";

/// The largest file `project.read` returns, and the largest content `project.write` accepts.
pub const MAX_READ_BYTES: usize = 32 * 1024;
pub const MAX_WRITE_BYTES: usize = 32 * 1024;
/// The longest project-relative path accepted.
pub const MAX_PATH_BYTES: usize = 200;

/// `project.list`: entries returned.
pub const MAX_LIST_ENTRIES: usize = 200;
/// `project.search`: the longest query, files examined, the largest file examined, matches
/// returned and the length of each line shown. All deterministic, none unlimited.
pub const MAX_QUERY_BYTES: usize = 200;
pub const MAX_SEARCH_FILES: usize = 500;
pub const MAX_SEARCH_FILE_BYTES: u64 = 256 * 1024;
pub const MAX_MATCHES: usize = 50;
pub const MAX_LINE_CHARS: usize = 200;
/// The most a list or search observation carries.
pub const MAX_OUTPUT_BYTES: usize = 16 * 1024;

/// Invariant names, as the audit reports them.
pub const PATH_ESCAPE: &str = "path_escape";
pub const OUT_OF_ROOT_WRITE: &str = "out_of_root_write";
pub const HOST_PATH_LEAK: &str = "host_path_leak";
pub const NAVIGATION_MISMATCH: &str = "navigation_mismatch";

/// Why a path is not acceptable. No variant carries a host path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    Empty,
    TooLong,
    Absolute,
    InvalidCharacter(char),
    /// An empty, `.` or `..` component.
    BadComponent(String),
    /// A name Chip never lets a model read or write (`.git`, `.env*`, its own temporary files).
    Reserved(String),
    /// A symbolic link on the path.
    Symlink(String),
    /// The resolved target is not inside the project root.
    EscapesRoot,
    /// An existing component is not a directory.
    NotADirectory(String),
    /// The filesystem could not be inspected.
    Unreadable,
}

impl std::fmt::Display for PathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "the path is empty"),
            Self::TooLong => write!(f, "the path is longer than {MAX_PATH_BYTES} bytes"),
            Self::Absolute => write!(f, "the path must be project-relative, not absolute"),
            Self::InvalidCharacter(c) => write!(
                f,
                "the path contains {c:?}; only letters, digits, '_', '-', '.' and '/' are allowed"
            ),
            Self::BadComponent(c) => write!(f, "the path has a {c:?} component"),
            Self::Reserved(c) => write!(f, "the path component {c:?} is reserved"),
            Self::Symlink(c) => write!(f, "the path component {c:?} is a symbolic link"),
            Self::EscapesRoot => write!(f, "the path resolves outside the project root"),
            Self::NotADirectory(c) => write!(f, "the path component {c:?} is not a directory"),
            Self::Unreadable => write!(f, "the path could not be inspected"),
        }
    }
}

/// The lexical rules, in one place. No filesystem access: a path that fails here never touches it.
fn components(path: &str) -> Result<Vec<&str>, PathError> {
    if path.is_empty() {
        return Err(PathError::Empty);
    }
    if path.len() > MAX_PATH_BYTES {
        return Err(PathError::TooLong);
    }
    if path.starts_with('/') {
        return Err(PathError::Absolute);
    }
    if let Some(c) = path
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/')))
    {
        return Err(PathError::InvalidCharacter(c));
    }
    let parts: Vec<&str> = path.split('/').collect();
    for part in &parts {
        if part.is_empty() || *part == "." || *part == ".." {
            return Err(PathError::BadComponent((*part).to_string()));
        }
        if *part == ".git" || part.starts_with(".env") || part.starts_with(".chip-write-") {
            return Err(PathError::Reserved((*part).to_string()));
        }
    }
    Ok(parts)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Missing,
    File,
    Directory,
    Other,
}

/// A path walked from the root, one component at a time, never through a symlink.
#[derive(Debug)]
struct Located {
    absolute: PathBuf,
    /// Every directory above the final component exists.
    parent_exists: bool,
    kind: Kind,
}

/// The project-file capabilities, confined to one root.
#[derive(Debug, Clone)]
pub struct ProjectExecutor {
    /// The canonical root. Established once, when the executor is made.
    root: Result<PathBuf, String>,
}

impl ProjectExecutor {
    pub fn new(root: impl AsRef<Path>) -> Self {
        let root = fs::canonicalize(root.as_ref())
            .map_err(|e| format!("the project root cannot be resolved ({})", e.kind()))
            .and_then(|r| {
                r.is_dir()
                    .then_some(r)
                    .ok_or_else(|| "the project root is not a directory".to_string())
            });
        Self { root }
    }

    fn root(&self) -> Result<&Path, String> {
        self.root.as_deref().map_err(Clone::clone)
    }

    fn locate(&self, path: &str) -> Result<Located, PathError> {
        let parts = components(path)?;
        let root = self.root().map_err(|_| PathError::Unreadable)?;
        let mut current = root.to_path_buf();
        let mut kind = Kind::Missing;
        let mut parent_exists = true;
        for (i, part) in parts.iter().enumerate() {
            let last = i + 1 == parts.len();
            current.push(part);
            match fs::symlink_metadata(&current) {
                Ok(m) if m.file_type().is_symlink() => {
                    return Err(PathError::Symlink((*part).to_string()));
                }
                Ok(m) => {
                    if !last && !m.is_dir() {
                        return Err(PathError::NotADirectory((*part).to_string()));
                    }
                    if last {
                        kind = if m.is_file() {
                            Kind::File
                        } else if m.is_dir() {
                            Kind::Directory
                        } else {
                            Kind::Other
                        };
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    parent_exists = last;
                    break;
                }
                Err(_) => return Err(PathError::Unreadable),
            }
        }
        // Belt and braces: the nearest existing ancestor, resolved for real, is inside the root.
        let mut anchor = current.clone();
        while !anchor.exists() {
            if !anchor.pop() {
                return Err(PathError::EscapesRoot);
            }
        }
        let real = fs::canonicalize(&anchor).map_err(|_| PathError::Unreadable)?;
        if !real.starts_with(root) {
            return Err(PathError::EscapesRoot);
        }
        Ok(Located {
            absolute: current,
            parent_exists,
            kind,
        })
    }
}

impl ProjectExecutor {
    /// The directory a navigation request names: the root itself for an absent path or exactly
    /// ".", otherwise the same checked walk as any other path. The root is never shown as a host
    /// path: it is ".".
    fn locate_dir(&self, path: Option<&str>) -> Result<Located, PathError> {
        match path {
            None | Some(".") => Ok(Located {
                absolute: self
                    .root()
                    .map_err(|_| PathError::Unreadable)?
                    .to_path_buf(),
                parent_exists: true,
                kind: Kind::Directory,
            }),
            Some(p) => self.locate(p),
        }
    }
}

fn text_input<'a>(
    inputs: &'a BTreeMap<String, InputValue>,
    name: &str,
) -> Result<&'a str, CapabilityError> {
    match inputs.get(name) {
        Some(InputValue::Text(s)) => Ok(s),
        Some(_) => Err(CapabilityError::InvalidInput(format!(
            "input '{name}' must be text"
        ))),
        None => Err(CapabilityError::InvalidInput(format!(
            "missing required input '{name}'"
        ))),
    }
}

fn input(name: &str, description: &str) -> CapabilityInput {
    CapabilityInput {
        name: name.to_string(),
        description: description.to_string(),
        required: true,
    }
}

fn optional_input(name: &str, description: &str) -> CapabilityInput {
    CapabilityInput {
        required: false,
        ..input(name, description)
    }
}

/// An input that may be absent; present, it must be text.
fn optional_text<'a>(
    inputs: &'a BTreeMap<String, InputValue>,
    name: &str,
) -> Result<Option<&'a str>, CapabilityError> {
    match inputs.get(name) {
        None => Ok(None),
        Some(InputValue::Text(s)) => Ok(Some(s)),
        Some(_) => Err(CapabilityError::InvalidInput(format!(
            "input '{name}' must be text"
        ))),
    }
}

/// A literal search query: short, one line, no control characters.
fn check_query(query: &str) -> Result<(), CapabilityError> {
    let bad = |why: String| Err(CapabilityError::InvalidInput(why));
    if query.is_empty() {
        return bad("the query is empty".into());
    }
    if query.len() > MAX_QUERY_BYTES {
        return bad(format!(
            "the query is {} bytes; the limit is {MAX_QUERY_BYTES}",
            query.len()
        ));
    }
    if query.chars().any(char::is_control) {
        return bad("the query contains a control character".into());
    }
    Ok(())
}

fn describe(
    id: &str,
    name: &str,
    description: &str,
    inputs: Vec<CapabilityInput>,
) -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::new(CapabilityId::new(id).unwrap(), name, description)
        .with_max_input_bytes(MAX_WRITE_BYTES)
        // The project changes between requests: a read or a write is performed, never remembered.
        .without_evidence_reuse();
    d.inputs = inputs;
    d
}

/// A navigation capability: short text inputs, performed again every time.
fn describe_navigation(
    id: &str,
    name: &str,
    description: &str,
    inputs: Vec<CapabilityInput>,
) -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::new(CapabilityId::new(id).unwrap(), name, description)
        .without_evidence_reuse();
    d.inputs = inputs;
    d
}

#[async_trait::async_trait]
impl CapabilityProvider for ProjectExecutor {
    async fn capabilities(&self) -> Result<Vec<CapabilityDescriptor>, CapabilityError> {
        Ok(vec![
            describe_navigation(
                PROJECT_LIST,
                "List a project directory",
                "List the files and directories directly inside a project directory; path is project-relative, or \".\" or absent for the project root.",
                vec![optional_input(
                    "path",
                    "project-relative directory, or \".\" for the root",
                )],
            ),
            describe_navigation(
                PROJECT_SEARCH,
                "Search project text",
                "Find lines containing a literal text in the project's text files; returns path:line: text. Optional path narrows it to a project-relative directory or file.",
                vec![
                    input("query", "the literal text to find"),
                    optional_input(
                        "path",
                        "project-relative directory or file to search, or \".\" for all",
                    ),
                ],
            ),
            describe(
                PROJECT_READ,
                "Read a project file",
                "Read a UTF-8 text file of the project by project-relative path, for example src/lib.rs.",
                vec![input("path", "project-relative path of the file")],
            ),
            describe(
                PROJECT_WRITE,
                "Write a project file",
                "Create or fully replace a UTF-8 text file of the project; content is the whole new file. The parent directory must exist.",
                vec![
                    input("path", "project-relative path of the file"),
                    input("content", "the complete new content of the file"),
                ],
            ),
        ])
    }

    async fn availability(&self, id: &CapabilityId) -> CapabilityAvailability {
        if !matches!(
            id.as_str(),
            PROJECT_READ | PROJECT_WRITE | PROJECT_LIST | PROJECT_SEARCH
        ) {
            return CapabilityAvailability::Unavailable(format!(
                "{id} is not a project capability"
            ));
        }
        match self.root() {
            Ok(_) => CapabilityAvailability::Available,
            Err(why) => CapabilityAvailability::Unavailable(why),
        }
    }

    async fn validate_inputs(
        &self,
        id: &CapabilityId,
        inputs: &BTreeMap<String, InputValue>,
    ) -> Result<(), CapabilityError> {
        let expected: &[&str] = match id.as_str() {
            PROJECT_READ => &["path"],
            PROJECT_WRITE => &["path", "content"],
            PROJECT_LIST => &["path"],
            PROJECT_SEARCH => &["query", "path"],
            other => return Err(CapabilityError::Unknown(other.to_string())),
        };
        for name in inputs.keys() {
            if !expected.contains(&name.as_str()) {
                return Err(CapabilityError::InvalidInput(format!(
                    "capability does not accept input '{name}'"
                )));
            }
        }
        if matches!(id.as_str(), PROJECT_LIST | PROJECT_SEARCH) {
            let path = optional_text(inputs, "path")?;
            self.locate_dir(path)
                .map_err(|e| CapabilityError::InvalidInput(e.to_string()))?;
            if id.as_str() == PROJECT_SEARCH {
                check_query(text_input(inputs, "query")?)?;
            }
            return Ok(());
        }
        let path = text_input(inputs, "path")?;
        self.locate(path)
            .map_err(|e| CapabilityError::InvalidInput(e.to_string()))?;
        if id.as_str() == PROJECT_WRITE {
            let content = text_input(inputs, "content")?;
            if content.len() > MAX_WRITE_BYTES {
                return Err(CapabilityError::InvalidInput(format!(
                    "content is {} bytes; the limit is {MAX_WRITE_BYTES}",
                    content.len()
                )));
            }
        }
        Ok(())
    }
}

/// One canonical JSON line: sorted keys, only values Chip produced.
fn line(fields: serde_json::Value) -> String {
    serde_json::to_string(&fields).expect("a JSON value serialises")
}

fn failure(capability: &str, path: &str, error: &str) -> (bool, String) {
    (
        false,
        line(serde_json::json!({"capability": capability, "path": path, "error": error})),
    )
}

fn io_code(e: &std::io::Error) -> &'static str {
    match e.kind() {
        std::io::ErrorKind::NotFound => "not_found",
        std::io::ErrorKind::PermissionDenied => "permission_denied",
        _ => "io_error",
    }
}

impl ProjectExecutor {
    /// Returns `(succeeded, observation text)`.
    fn read(&self, path: &str) -> Result<(bool, String), PathError> {
        let located = self.locate(path)?;
        match located.kind {
            Kind::Missing => return Ok(failure(PROJECT_READ, path, "not_found")),
            Kind::Directory | Kind::Other => return Ok(failure(PROJECT_READ, path, "not_a_file")),
            Kind::File => {}
        }
        let mut file = match fs::File::open(&located.absolute) {
            Ok(f) => f,
            Err(e) => return Ok(failure(PROJECT_READ, path, io_code(&e))),
        };
        let mut bytes = Vec::new();
        if let Err(e) = (&mut file)
            .take(MAX_READ_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
        {
            return Ok(failure(PROJECT_READ, path, io_code(&e)));
        }
        if bytes.len() > MAX_READ_BYTES {
            return Ok(failure(PROJECT_READ, path, "too_large"));
        }
        let Ok(content) = String::from_utf8(bytes) else {
            return Ok(failure(PROJECT_READ, path, "not_utf8"));
        };
        let head = line(serde_json::json!({
            "capability": PROJECT_READ,
            "path": path,
            "bytes": content.len(),
            "sha256": hex(&Sha256::digest(content.as_bytes())),
        }));
        Ok((true, format!("{head}\n--- content ---\n{content}")))
    }

    fn write(&self, path: &str, content: &str) -> Result<(bool, String), PathError> {
        self.write_with(path, content, &|p| fs::read(p))
    }

    /// `read_back` is how the target is read after the write: the real filesystem in production, a
    /// stand-in in the one test that must show a write that did not land is not reported as one.
    fn write_with(
        &self,
        path: &str,
        content: &str,
        read_back: &dyn Fn(&Path) -> std::io::Result<Vec<u8>>,
    ) -> Result<(bool, String), PathError> {
        if content.len() > MAX_WRITE_BYTES {
            return Err(PathError::TooLong);
        }
        let located = self.locate(path)?;
        if !located.parent_exists {
            return Ok(failure(PROJECT_WRITE, path, "parent_missing"));
        }
        let existing = match located.kind {
            Kind::Missing => None,
            Kind::File => Some(&located.absolute),
            Kind::Directory => return Ok(failure(PROJECT_WRITE, path, "is_a_directory")),
            Kind::Other => return Ok(failure(PROJECT_WRITE, path, "not_a_file")),
        };
        let previous = existing.and_then(|p| fs::read(p).ok());
        let mode = existing.and_then(|p| file_mode(p));
        if let Err(e) = atomic_write(&located.absolute, content.as_bytes(), mode) {
            return Ok(failure(PROJECT_WRITE, path, io_code(&e)));
        }
        // Success is what the filesystem now holds, not that a write was attempted.
        match read_back(&located.absolute) {
            Ok(after) if after == content.as_bytes() => {}
            Ok(_) => return Ok(failure(PROJECT_WRITE, path, "readback_mismatch")),
            Err(e) => return Ok(failure(PROJECT_WRITE, path, io_code(&e))),
        }
        let changed = previous.as_deref() != Some(content.as_bytes());
        let head = line(serde_json::json!({
            "capability": PROJECT_WRITE,
            "path": path,
            "operation": if existing.is_some() { "replaced" } else { "created" },
            "bytes_written": content.len(),
            "changed": changed,
            "sha256": hex(&Sha256::digest(content.as_bytes())),
        }));
        Ok((true, head))
    }
}

/// A directory's entries as Chip lists them: no symlinks, nothing reserved, and nothing whose name
/// the path rules would not let a model address anyway. Sorted by name.
fn visible_entries(dir: &Path) -> std::io::Result<(Vec<(String, fs::Metadata)>, usize, usize)> {
    let (mut entries, mut symlinks, mut unaddressable) = (Vec::new(), 0, 0);
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let Ok(name) = entry.file_name().into_string() else {
            unaddressable += 1;
            continue;
        };
        let meta = fs::symlink_metadata(entry.path())?;
        if meta.file_type().is_symlink() {
            symlinks += 1;
        } else if components(&name).is_err() {
            unaddressable += 1;
        } else if meta.is_dir() || meta.is_file() {
            entries.push((name, meta));
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok((entries, symlinks, unaddressable))
}

fn child(base: Option<&str>, name: &str) -> String {
    match base {
        Some(b) => format!("{b}/{name}"),
        None => name.to_string(),
    }
}

/// The state of one search, with every bound it keeps.
struct Search<'a> {
    query: &'a str,
    files: usize,
    rows: Vec<String>,
    bytes: usize,
    truncated: bool,
    non_utf8: usize,
    large: usize,
}

impl Search<'_> {
    fn file(&mut self, path: &Path, rel: &str, len: u64) {
        if self.files >= MAX_SEARCH_FILES {
            self.truncated = true;
            return;
        }
        if len > MAX_SEARCH_FILE_BYTES {
            self.large += 1;
            return;
        }
        self.files += 1;
        let Ok(bytes) = fs::read(path) else { return };
        let Ok(text) = String::from_utf8(bytes) else {
            self.non_utf8 += 1;
            return;
        };
        for (i, line) in text.lines().enumerate() {
            if !line.contains(self.query) {
                continue;
            }
            let shown: String = line.chars().take(MAX_LINE_CHARS).collect();
            let row = format!("{rel}:{}: {shown}", i + 1);
            if self.rows.len() >= MAX_MATCHES || self.bytes + row.len() + 1 > MAX_OUTPUT_BYTES {
                self.truncated = true;
                return;
            }
            self.bytes += row.len() + 1;
            self.rows.push(row);
        }
    }

    fn directory(&mut self, dir: &Path, rel: Option<&str>) -> std::io::Result<()> {
        let (entries, _, _) = visible_entries(dir)?;
        for (name, meta) in entries {
            if self.truncated {
                return Ok(());
            }
            // Hidden entries and build output are not searched (an explicitly named directory is).
            if name.starts_with('.') {
                continue;
            }
            let path = dir.join(&name);
            let rel = child(rel, &name);
            if meta.is_dir() {
                if name != "target" && name != "node_modules" {
                    self.directory(&path, Some(&rel))?;
                }
            } else {
                self.file(&path, &rel, meta.len());
            }
        }
        Ok(())
    }
}

impl ProjectExecutor {
    fn list(&self, path: Option<&str>) -> Result<(bool, String), PathError> {
        let shown = path.unwrap_or(".");
        let located = self.locate_dir(path)?;
        match located.kind {
            Kind::Missing => return Ok(failure(PROJECT_LIST, shown, "not_found")),
            Kind::File | Kind::Other => return Ok(failure(PROJECT_LIST, shown, "not_a_directory")),
            Kind::Directory => {}
        }
        let base = (shown != ".").then_some(shown);
        let (entries, symlinks, unaddressable) = match visible_entries(&located.absolute) {
            Ok(found) => found,
            Err(e) => return Ok(failure(PROJECT_LIST, shown, io_code(&e))),
        };
        let (mut rows, mut bytes, mut truncated) = (Vec::new(), 0usize, false);
        for (name, meta) in &entries {
            let rel = child(base, name);
            let row = if meta.is_dir() {
                format!("dir {rel}")
            } else {
                format!("file {rel} {}", meta.len())
            };
            if rows.len() >= MAX_LIST_ENTRIES || bytes + row.len() + 1 > MAX_OUTPUT_BYTES {
                truncated = true;
                break;
            }
            bytes += row.len() + 1;
            rows.push(row);
        }
        let head = line(serde_json::json!({
            "capability": PROJECT_LIST,
            "path": shown,
            "entries": rows.len(),
            "truncated": truncated,
            "skipped_symlinks": symlinks,
            "skipped_unaddressable": unaddressable,
        }));
        Ok((
            true,
            format!("{head}\n--- entries ---\n{}", rows.join("\n")),
        ))
    }

    fn search(&self, query: &str, path: Option<&str>) -> Result<(bool, String), PathError> {
        let shown = path.unwrap_or(".");
        let located = self.locate_dir(path)?;
        let mut search = Search {
            query,
            files: 0,
            rows: Vec::new(),
            bytes: 0,
            truncated: false,
            non_utf8: 0,
            large: 0,
        };
        match located.kind {
            Kind::Missing => return Ok(failure(PROJECT_SEARCH, shown, "not_found")),
            Kind::Other => return Ok(failure(PROJECT_SEARCH, shown, "not_a_file_or_directory")),
            Kind::File => {
                let len = fs::metadata(&located.absolute)
                    .map(|m| m.len())
                    .unwrap_or(0);
                search.file(&located.absolute, shown, len);
            }
            Kind::Directory => {
                let base = (shown != ".").then_some(shown);
                if let Err(e) = search.directory(&located.absolute, base) {
                    return Ok(failure(PROJECT_SEARCH, shown, io_code(&e)));
                }
            }
        }
        let head = line(serde_json::json!({
            "capability": PROJECT_SEARCH,
            "query": query,
            "path": shown,
            "files_examined": search.files,
            "matches": search.rows.len(),
            "truncated": search.truncated,
            "skipped_non_utf8": search.non_utf8,
            "skipped_large": search.large,
        }));
        Ok((
            true,
            format!("{head}\n--- matches ---\n{}", search.rows.join("\n")),
        ))
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(unix)]
fn file_mode(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .ok()
        .map(|m| m.permissions().mode() & 0o777)
}

#[cfg(not(unix))]
fn file_mode(_path: &Path) -> Option<u32> {
    None
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Writes `content` next to `target` and renames it over `target`: the target is either what it was
/// or exactly `content`, never a prefix of it. A failure removes the temporary file.
fn atomic_write(target: &Path, content: &[u8], mode: Option<u32>) -> std::io::Result<()> {
    let parent = target
        .parent()
        .ok_or_else(|| std::io::Error::other("no parent directory"))?;
    let temporary = parent.join(format!(
        ".chip-write-{}-{}.tmp",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(content)?;
        file.sync_all()?;
        drop(file);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                &temporary,
                fs::Permissions::from_mode(mode.unwrap_or(0o644)),
            )?;
        }
        #[cfg(not(unix))]
        let _ = mode;
        fs::rename(&temporary, target)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    } else if let Ok(dir) = fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    result
}

#[async_trait::async_trait]
impl Executor for ProjectExecutor {
    async fn execute(&self, request: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        let capability = request.intent.as_str();
        let invalid = |why: String| ExecutionError::InvalidRequest(why);
        if !matches!(
            capability,
            PROJECT_READ | PROJECT_WRITE | PROJECT_LIST | PROJECT_SEARCH
        ) {
            return Err(invalid(format!(
                "'{capability}' is not a capability this executor provides"
            )));
        }
        if self.root().is_err() {
            return Err(ExecutionError::ExecutorUnavailable(
                "the project root is unavailable".into(),
            ));
        }
        // Re-validated at the moment of use: the executor does not rely on having been asked
        // nicely by whoever validated first.
        let id = CapabilityId::new(capability).map_err(|e| invalid(e.to_string()))?;
        self.validate_inputs(&id, &request.inputs)
            .await
            .map_err(|e| invalid(e.to_string()))?;
        if matches!(capability, PROJECT_LIST | PROJECT_SEARCH) {
            let path = optional_text(&request.inputs, "path")
                .map_err(|e| invalid(e.to_string()))?
                .map(str::to_string);
            let query = (capability == PROJECT_SEARCH)
                .then(|| text_input(&request.inputs, "query").map(str::to_string))
                .transpose()
                .map_err(|e| invalid(e.to_string()))?;
            let this = self.clone();
            let done = tokio::task::spawn_blocking(move || match query {
                Some(q) => this.search(&q, path.as_deref()),
                None => this.list(path.as_deref()),
            })
            .await
            .map_err(|_| {
                ExecutionError::ExecutionFailed("the file operation did not complete".into())
            })?
            .map_err(|e| invalid(e.to_string()))?;
            return Ok(match done {
                (true, text) => ExecutionResult::success(request.id, text),
                (false, text) => ExecutionResult::failure(request.id, text),
            });
        }
        let path = text_input(&request.inputs, "path")
            .map_err(|e| invalid(e.to_string()))?
            .to_string();
        let content = (capability == PROJECT_WRITE)
            .then(|| text_input(&request.inputs, "content").map(str::to_string))
            .transpose()
            .map_err(|e| invalid(e.to_string()))?;
        let this = self.clone();
        let done = tokio::task::spawn_blocking(move || match content {
            Some(content) => this.write(&path, &content),
            None => this.read(&path),
        })
        .await
        .map_err(|_| ExecutionError::ExecutionFailed("the file operation did not complete".into()))?
        .map_err(|e| invalid(e.to_string()))?;
        Ok(match done {
            (true, text) => ExecutionResult::success(request.id, text),
            (false, text) => ExecutionResult::failure(request.id, text),
        })
    }
}

// ---- what a recorded observation says, and the audit's independent check of it ---------------------

/// The canonical first line of a project observation, parsed.
fn project_line(observation: &Observation) -> Option<serde_json::Value> {
    let first = observation.output.as_deref()?.lines().next()?;
    let value: serde_json::Value = serde_json::from_str(first).ok()?;
    matches!(
        value.get("capability")?.as_str()?,
        PROJECT_READ | PROJECT_WRITE | PROJECT_LIST | PROJECT_SEARCH
    )
    .then_some(value)
}

/// What a project observation says about one successful write, for utility reporting: the path,
/// and whether the write changed the file.
pub fn write_summary(observation: &Observation) -> Option<(String, bool)> {
    if observation.kind != ObservationKind::ExecutionCompleted {
        return None;
    }
    let v = project_line(observation)?;
    (v["capability"] == PROJECT_WRITE).then(|| {
        (
            v["path"].as_str().unwrap_or_default().to_string(),
            v["changed"] == true,
        )
    })
}

/// Checks a recorded path with its own, separate reading of the rules: not trusting that the
/// executor applied them.
fn lexically_inside(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.contains('\0')
        && path
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != "..")
}

#[derive(Debug)]
struct PathEscape {
    root: PathBuf,
}

impl ObservationInvariant for PathEscape {
    fn name(&self) -> &'static str {
        PATH_ESCAPE
    }

    /// A project observation naming a path that is not project-relative and inside, a listing or
    /// search that returns one, or a read, listing or search that really came from outside the
    /// root.
    fn violations(&self, observation: &Observation) -> usize {
        let Some(v) = project_line(observation) else {
            return 0;
        };
        let capability = v["capability"].as_str().unwrap_or_default();
        let navigation = matches!(capability, PROJECT_LIST | PROJECT_SEARCH);
        let path = v["path"].as_str().unwrap_or_default();
        if !(lexically_inside(path) || (navigation && path == ".")) {
            return 1;
        }
        if navigation {
            let outside = returned_paths(observation, capability)
                .iter()
                .filter(|p| !lexically_inside(p))
                .count();
            if outside > 0 {
                return outside;
            }
        }
        let succeeded = observation.kind == ObservationKind::ExecutionCompleted;
        let reads_from_disk = capability == PROJECT_READ || navigation;
        usize::from(succeeded && reads_from_disk && !really_inside(&self.root, path))
    }
}

/// The paths a listing or a search observation returns, from its body.
fn returned_paths(observation: &Observation, capability: &str) -> Vec<String> {
    let body = observation
        .output
        .as_deref()
        .map(|t| t.lines().skip(2))
        .into_iter()
        .flatten();
    body.filter_map(|row| match capability {
        PROJECT_LIST => row.split(' ').nth(1).map(str::to_string),
        PROJECT_SEARCH => row.split(':').next().map(str::to_string),
        _ => None,
    })
    .collect()
}

#[derive(Debug)]
struct OutOfRootWrite {
    root: PathBuf,
}

impl ObservationInvariant for OutOfRootWrite {
    fn name(&self) -> &'static str {
        OUT_OF_ROOT_WRITE
    }

    /// A successful write whose target, resolved for real, is not inside the root (or that cannot
    /// be shown to be: it fails closed).
    fn violations(&self, observation: &Observation) -> usize {
        let Some(v) = project_line(observation) else {
            return 0;
        };
        let wrote = v["capability"] == PROJECT_WRITE
            && observation.kind == ObservationKind::ExecutionCompleted;
        let path = v["path"].as_str().unwrap_or_default();
        usize::from(wrote && !(lexically_inside(path) && really_inside(&self.root, path)))
    }
}

/// No project observation contains the host path of the project root.
#[derive(Debug)]
struct HostPathLeak {
    spellings: Vec<String>,
}

impl ObservationInvariant for HostPathLeak {
    fn name(&self) -> &'static str {
        HOST_PATH_LEAK
    }

    fn violations(&self, observation: &Observation) -> usize {
        if project_line(observation).is_none() {
            return 0;
        }
        let text = observation.output.as_deref().unwrap_or_default();
        usize::from(self.spellings.iter().any(|s| text.contains(s.as_str())))
    }
}

/// Listings and searches match the real filesystem, read again by the audit itself.
///
/// Every listed entry and every reported match is checked against the files as they are. A scope
/// that a later *successful write* in the same trajectory touched is not checked: that change
/// legitimately outdates what the earlier observation said.
#[derive(Debug)]
struct NavigationMismatch {
    root: PathBuf,
}

impl NavigationMismatch {
    /// What a listing of `dir` should hold, as `kind path size` rows, read independently.
    fn real_entries(&self, shown: &str) -> Option<std::collections::BTreeSet<String>> {
        let dir = if shown == "." {
            self.root.clone()
        } else {
            self.root.join(shown)
        };
        let mut rows = std::collections::BTreeSet::new();
        for entry in fs::read_dir(dir).ok()? {
            let entry = entry.ok()?;
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            let meta = fs::symlink_metadata(entry.path()).ok()?;
            if meta.file_type().is_symlink() || components(&name).is_err() {
                continue;
            }
            let rel = if shown == "." {
                name
            } else {
                format!("{shown}/{name}")
            };
            if meta.is_dir() {
                rows.insert(format!("dir {rel}"));
            } else if meta.is_file() {
                rows.insert(format!("file {rel} {}", meta.len()));
            }
        }
        Some(rows)
    }

    fn check_listing(
        &self,
        o: &Observation,
        v: &serde_json::Value,
        later: &[String],
        foreign_later: bool,
    ) -> usize {
        let shown = v["path"].as_str().unwrap_or(".");
        let parent_of = |p: &str| p.rsplit_once('/').map_or(".", |(d, _)| d).to_string();
        if later.iter().any(|w| parent_of(w) == shown) {
            return 0;
        }
        if foreign_later {
            // Something other than a project capability ran afterwards (a test run can create and
            // remove files), so the set may legitimately have changed: every entry listed must
            // still be what it was said to be, and no more is claimed.
            let wrong = o
                .output
                .as_deref()
                .unwrap_or_default()
                .lines()
                .skip(2)
                .filter(|row| {
                    let mut parts = row.split(' ');
                    let (kind, rel) = (parts.next(), parts.next());
                    let Some(rel) = rel.filter(|r| lexically_inside(r)) else {
                        return true;
                    };
                    match fs::symlink_metadata(self.root.join(rel)) {
                        Ok(m) => {
                            m.file_type().is_symlink()
                                || (kind == Some("dir")) != m.is_dir()
                                || (kind == Some("file")) != m.is_file()
                        }
                        Err(_) => true,
                    }
                })
                .count();
            return usize::from(wrong > 0);
        }
        let Some(real) = self.real_entries(shown) else {
            return 1;
        };
        let observed: std::collections::BTreeSet<String> = o
            .output
            .as_deref()
            .map(|t| t.lines().skip(2).map(str::to_string).collect())
            .unwrap_or_default();
        let ok = if v["truncated"] == true {
            observed.is_subset(&real)
        } else {
            observed == real
        };
        usize::from(!ok)
    }

    fn check_search(
        &self,
        o: &Observation,
        v: &serde_json::Value,
        later: &[String],
        foreign_later: bool,
    ) -> usize {
        let query = v["query"].as_str().unwrap_or_default();
        let mut bad = 0;
        for row in o.output.as_deref().unwrap_or_default().lines().skip(2) {
            let mut parts = row.splitn(3, ':');
            let (Some(rel), Some(number), Some(text)) = (parts.next(), parts.next(), parts.next())
            else {
                bad += 1;
                continue;
            };
            if later.iter().any(|w| w == rel) {
                continue;
            }
            if foreign_later {
                // The file's text may have changed since; it must at least still be a real file
                // of the project.
                let real = lexically_inside(rel)
                    && really_inside(&self.root, rel)
                    && fs::metadata(self.root.join(rel)).is_ok_and(|m| m.is_file());
                bad += usize::from(!real);
                continue;
            }
            let shown = text.strip_prefix(' ').unwrap_or(text);
            let real_line = number
                .parse::<usize>()
                .ok()
                .filter(|n| *n >= 1 && lexically_inside(rel) && really_inside(&self.root, rel))
                .and_then(|n| {
                    let content = fs::read_to_string(self.root.join(rel)).ok()?;
                    content.lines().nth(n - 1).map(str::to_string)
                });
            let genuine = real_line.is_some_and(|l| {
                l.contains(query) && l.chars().take(MAX_LINE_CHARS).collect::<String>() == shown
            });
            bad += usize::from(!genuine);
        }
        bad
    }
}

impl ObservationInvariant for NavigationMismatch {
    fn name(&self) -> &'static str {
        NAVIGATION_MISMATCH
    }

    fn violations(&self, _observation: &Observation) -> usize {
        0
    }

    fn violations_in_trajectory(&self, observations: &[Observation]) -> usize {
        let mut bad = 0;
        for (i, o) in observations.iter().enumerate() {
            if o.kind != ObservationKind::ExecutionCompleted {
                continue;
            }
            let Some(v) = project_line(o) else { continue };
            let later: Vec<String> = observations[i + 1..]
                .iter()
                .filter_map(write_summary)
                .map(|(path, _)| path)
                .collect();
            // An execution that is not a project capability (a test run, say) may have changed the
            // project in ways the audit cannot see.
            let foreign_later = observations[i + 1..]
                .iter()
                .any(|later| project_line(later).is_none());
            match v["capability"].as_str() {
                Some(PROJECT_LIST) => bad += self.check_listing(o, &v, &later, foreign_later),
                Some(PROJECT_SEARCH) => bad += self.check_search(o, &v, &later, foreign_later),
                _ => {}
            }
        }
        bad
    }
}

fn really_inside(root: &Path, path: &str) -> bool {
    let (Ok(root), Ok(real)) = (fs::canonicalize(root), fs::canonicalize(root.join(path))) else {
        return false;
    };
    real.starts_with(root)
}

/// Audit: no project observation names a path outside the project.
pub fn path_escape_invariant(root: impl AsRef<Path>) -> Arc<dyn ObservationInvariant> {
    Arc::new(PathEscape {
        root: root.as_ref().to_path_buf(),
    })
}

/// Audit: no project observation contains the host path of the project root.
pub fn host_path_leak_invariant(root: impl AsRef<Path>) -> Arc<dyn ObservationInvariant> {
    let given = root.as_ref().to_string_lossy().into_owned();
    let mut spellings = vec![given];
    if let Ok(real) = fs::canonicalize(root.as_ref()) {
        spellings.push(real.to_string_lossy().into_owned());
    }
    spellings.retain(|s| s.len() > 1);
    spellings.dedup();
    Arc::new(HostPathLeak { spellings })
}

/// Audit: listings and searches report what the filesystem really holds.
pub fn navigation_mismatch_invariant(root: impl AsRef<Path>) -> Arc<dyn ObservationInvariant> {
    Arc::new(NavigationMismatch {
        root: root.as_ref().to_path_buf(),
    })
}

/// Audit: no successful write landed outside the project root.
pub fn out_of_root_write_invariant(root: impl AsRef<Path>) -> Arc<dyn ObservationInvariant> {
    Arc::new(OutOfRootWrite {
        root: root.as_ref().to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lexical_rules_alone_refuse_every_unsafe_path() {
        for (path, expected) in [
            ("", PathError::Empty),
            ("/etc/passwd", PathError::Absolute),
            ("../x", PathError::BadComponent("..".into())),
            ("a/../b", PathError::BadComponent("..".into())),
            ("a/./b", PathError::BadComponent(".".into())),
            ("a//b", PathError::BadComponent("".into())),
            (".git/config", PathError::Reserved(".git".into())),
            (".env.local", PathError::Reserved(".env.local".into())),
        ] {
            assert_eq!(components(path).unwrap_err(), expected, "{path:?}");
        }
        assert_eq!(components("src/lib.rs").unwrap(), ["src", "lib.rs"]);
    }

    #[test]
    fn a_write_that_did_not_land_as_requested_is_not_reported_as_one() {
        let root =
            std::env::temp_dir().join(format!("chip-project-readback-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let p = ProjectExecutor::new(&root);
        let (ok, text) = p
            .write_with("a.txt", "wanted", &|_| Ok(b"something else".to_vec()))
            .unwrap();
        assert!(!ok, "{text}");
        assert!(
            text.contains("readback_mismatch") && !text.contains("bytes_written"),
            "{text}"
        );
        let (ok, text) = p
            .write_with("b.txt", "wanted", &|_| Err(std::io::Error::other("gone")))
            .unwrap();
        assert!(!ok && text.contains("io_error"), "{text}");
        assert!(
            p.write("c.txt", "wanted").unwrap().0,
            "the real read-back agrees"
        );
    }
}
