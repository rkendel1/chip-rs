//! `project.observe`: a bounded, deterministic observation of project structure, made by PAX.
//!
//! This is project *observation*, not agency (`docs/product/project-observation-boundary.md`). PAX
//! reads the project's manifests and Rust source and states structural facts (which files exist,
//! which module contains which, where a declaration is) with provenance. It says nothing about
//! which fact matters to a goal, and nothing this capability returns establishes that work is done.
//!
//! # Authority
//!
//! * **The model** may name `project.observe` and supply one `scope`, validated here before anything
//!   runs. It supplies no path outside the project, no command, no argument, no limit and no fact.
//! * **Chip** validates the scope grammar and the project-root containment (lexically and through
//!   the filesystem, refusing every symlink), fixes the argument vector and every bound, verifies
//!   the PAX release, parses PAX's document strictly, re-verifies that every artifact PAX names is
//!   inside the project, and renders the observation itself. PAX's checks are not a substitute for
//!   Chip's.
//! * **PAX** reads the artifacts and states the facts, in `pax.observation.v1` and nothing else.
//!   This adapter does not use `pax graph`, `pax info`, `pax reality` or `pax drift`, and reads no
//!   field the contract does not define.
//!
//! # The observation is a state, not a work outcome
//!
//! `complete`, `partial`, `invalid_scope`, `limit_exceeded`, `artifact_unreadable`, `unsupported`
//! and `malformed` describe what was observed. `partial` is never rendered as complete, and
//! "not observed" is never rendered as "absent". A failure to run PAX at all produces **no
//! observation**, not an empty one, and PAX's stderr is only ever a bounded diagnostic.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use chip_core::{
    CapabilityAvailability, CapabilityDescriptor, CapabilityError, CapabilityId, CapabilityInput,
    CapabilityProvider, ExecutionError, ExecutionRequest, ExecutionResult, Executor, InputValue,
};
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::{PaxExecutor, Version};

/// The capability this adapter provides.
pub const PROJECT_OBSERVE_CAPABILITY: &str = "project.observe";

/// The only document this adapter understands. Anything else is rejected, never adapted.
pub const OBSERVATION_SCHEMA: &str = "pax.observation.v1";

/// A released PAX, exactly as published. These come from the release, not from inference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaxRelease {
    pub version: &'static str,
    pub tag: &'static str,
    pub commit: &'static str,
}

/// The PAX release Chip is built, tested and benchmarked against. CI installs exactly this tag and
/// checks exactly this commit.
pub const PINNED_PAX: PaxRelease = PaxRelease {
    version: "0.4.1",
    tag: "v0.4.1",
    commit: "674f3b3143874d1a692aca103b33f89da31a82ac",
};

/// The oldest PAX that provides [`OBSERVATION_SCHEMA`] as released. The version is the
/// compatibility contract: support is never detected by running an unsupported command.
pub const MIN_OBSERVE_PAX_VERSION: (u64, u64, u64) = (0, 4, 1);

// ---- bounds: fixed, Chip's, never chosen by a model ---------------------------------------------

/// Files PAX may read or list for one observation (`--max-files`).
pub const MAX_FILES: u64 = 50;
/// Source bytes PAX may read for one observation (`--max-bytes`).
pub const MAX_BYTES: u64 = 1024 * 1024;
/// Facts PAX may return for one observation (`--max-facts`).
pub const MAX_FACTS: u64 = 600;
/// The most bytes of observation the model is shown.
pub const MAX_RENDERED_BYTES: usize = 16 * 1024;
/// The most stdout Chip will hold from PAX. A longer document is never trusted whole.
pub const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const MAX_DIAGNOSTIC_BYTES: usize = 300;
/// A scope may be at most this long.
pub const MAX_SCOPE_BYTES: usize = 200;
const MAX_NAME_BYTES: usize = 64;
const MAX_MODULE_DEPTH: usize = 16;
const MAX_FIELD_BYTES: usize = 1024;
const TIMEOUT: Duration = Duration::from_secs(30);

// ---- states ---------------------------------------------------------------------------------------

/// What was observed. Not a work outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationState {
    /// PAX observed the scope, with nothing it could not establish, and all of it is shown.
    Complete,
    /// Some facts were observed and the observation is incomplete: PAX reported what it could not
    /// establish, or not all observed facts fit the rendering bound.
    Partial,
    /// PAX found no such scope, or the scope is not valid for it.
    InvalidScope,
    /// A bound was exceeded. Nothing is known about the scope from this result.
    LimitExceeded,
    /// PAX could not read an artifact it needed.
    ArtifactUnreadable,
    /// PAX does not support this scope or project.
    Unsupported,
    /// PAX's output was not a valid `pax.observation.v1` or broke Chip's boundary. It is not
    /// an observation.
    Malformed,
}

impl ObservationState {
    pub fn name(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial => "partial",
            Self::InvalidScope => "invalid_scope",
            Self::LimitExceeded => "limit_exceeded",
            Self::ArtifactUnreadable => "artifact_unreadable",
            Self::Unsupported => "unsupported",
            Self::Malformed => "malformed",
        }
    }

    pub const ALL: [ObservationState; 7] = [
        Self::Complete,
        Self::Partial,
        Self::InvalidScope,
        Self::LimitExceeded,
        Self::ArtifactUnreadable,
        Self::Unsupported,
        Self::Malformed,
    ];
}

/// The state a `project.observe` observation records in its first line, or `None` if `output` is not
/// one of this capability's observations. Reads Chip's own head line and nothing else.
pub fn parse_state(output: &str) -> Option<ObservationState> {
    let head: Value = serde_json::from_str(output.lines().next()?).ok()?;
    if head.get("capability")?.as_str()? != PROJECT_OBSERVE_CAPABILITY {
        return None;
    }
    let state = head.get("state")?.as_str()?;
    ObservationState::ALL
        .into_iter()
        .find(|s| s.name() == state)
}

// ---- scope ------------------------------------------------------------------------------------------

/// A request Chip will not pass to PAX. No variant carries a host path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeError {
    Empty,
    TooLong,
    /// A character outside printable ASCII without spaces.
    InvalidCharacter(char),
    /// Not one of `file:`, `path:`, `crate:`, `module:`.
    UnknownForm(String),
    /// The form is right and the rest of the scope is not.
    Malformed(&'static str),
    Absolute,
    /// An empty, `.` or `..` component.
    BadComponent(String),
    /// A name Chip never lets a model read: `.git`, `.env*`.
    Reserved(String),
    /// A symbolic link on the path. Refused whether it points inside the project or out of it.
    Symlink(String),
    /// The resolved target is outside the project root.
    EscapesRoot,
    /// The filesystem could not be inspected.
    Unreadable,
}

impl std::fmt::Display for ScopeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "the scope is empty"),
            Self::TooLong => write!(f, "the scope is longer than {MAX_SCOPE_BYTES} bytes"),
            Self::InvalidCharacter(c) => {
                write!(
                    f,
                    "the scope contains {c:?}; only printable ASCII without spaces is allowed"
                )
            }
            Self::UnknownForm(form) => write!(
                f,
                "unknown scope form {form:?}; use file:<path>, path:<prefix>, crate:<package>[/lib|/<bin|test|bench|example>/<name>] or module:<package>/lib::crate[::<module>...]"
            ),
            Self::Malformed(why) => write!(f, "malformed scope: {why}"),
            Self::Absolute => write!(f, "the path must be project-relative, not absolute"),
            Self::BadComponent(c) => write!(f, "the path has a {c:?} component"),
            Self::Reserved(c) => write!(f, "the path component {c:?} is reserved"),
            Self::Symlink(c) => write!(f, "the path component {c:?} is a symbolic link"),
            Self::EscapesRoot => write!(f, "the path resolves outside the project root"),
            Self::Unreadable => write!(f, "the path could not be inspected"),
        }
    }
}

/// A crate target as PAX names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Lib,
    Other { kind: String, name: String },
}

impl Target {
    fn render(&self) -> String {
        match self {
            Self::Lib => "lib".to_string(),
            Self::Other { kind, name } => format!("{kind}/{name}"),
        }
    }
}

/// The documented `pax.observation.v1` scope forms Chip accepts: nothing broader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// One `.rs` file, read in isolation.
    File(String),
    /// `.rs` files and manifests under a path prefix.
    Path(String),
    /// One workspace package, or one target of it.
    Crate {
        package: String,
        target: Option<Target>,
    },
    /// One module subtree of a crate target.
    Module {
        package: String,
        target: Target,
        modules: Vec<String>,
    },
}

fn is_name(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= MAX_NAME_BYTES
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    text.len() <= MAX_NAME_BYTES
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The lexical rules for a project-relative path a model names. They mirror the project
/// capabilities' rules: no filesystem access, and a path that fails here never touches it.
fn lexical(path: &str) -> Result<Vec<&str>, ScopeError> {
    if path.is_empty() {
        return Err(ScopeError::Malformed("the path is empty"));
    }
    if path.starts_with('/') {
        return Err(ScopeError::Absolute);
    }
    let parts: Vec<&str> = path.split('/').collect();
    for part in &parts {
        if part.is_empty() || *part == "." || *part == ".." {
            return Err(ScopeError::BadComponent((*part).to_string()));
        }
        if !part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        {
            let bad = part
                .chars()
                .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')))
                .unwrap_or('?');
            return Err(ScopeError::InvalidCharacter(bad));
        }
        if *part == ".git" || part.starts_with(".env") {
            return Err(ScopeError::Reserved((*part).to_string()));
        }
    }
    Ok(parts)
}

fn target(parts: &[&str]) -> Result<Target, ScopeError> {
    match parts {
        ["lib"] => Ok(Target::Lib),
        [kind @ ("bin" | "test" | "bench" | "example"), name] if is_name(name) => {
            Ok(Target::Other {
                kind: (*kind).to_string(),
                name: (*name).to_string(),
            })
        }
        _ => Err(ScopeError::Malformed(
            "a crate target is lib or <bin|test|bench|example>/<name>",
        )),
    }
}

impl Scope {
    /// Parses a scope. Only the documented forms are accepted; anything else fails closed.
    pub fn parse(text: &str) -> Result<Self, ScopeError> {
        if text.is_empty() {
            return Err(ScopeError::Empty);
        }
        if text.len() > MAX_SCOPE_BYTES {
            return Err(ScopeError::TooLong);
        }
        if let Some(c) = text.chars().find(|c| !c.is_ascii_graphic()) {
            return Err(ScopeError::InvalidCharacter(c));
        }
        let (form, rest) = text
            .split_once(':')
            .ok_or_else(|| ScopeError::UnknownForm(text.chars().take(20).collect()))?;
        match form {
            "file" => {
                lexical(rest)?;
                if !rest.ends_with(".rs") {
                    return Err(ScopeError::Malformed("a file scope names a .rs file"));
                }
                Ok(Self::File(rest.to_string()))
            }
            "path" => {
                lexical(rest)?;
                Ok(Self::Path(rest.to_string()))
            }
            "crate" => {
                let parts: Vec<&str> = rest.split('/').collect();
                let package = *parts.first().unwrap_or(&"");
                if !is_name(package) {
                    return Err(ScopeError::Malformed(
                        "the crate scope needs a package name",
                    ));
                }
                let target = match &parts[1..] {
                    [] => None,
                    rest => Some(target(rest)?),
                };
                Ok(Self::Crate {
                    package: package.to_string(),
                    target,
                })
            }
            "module" => {
                let segments: Vec<&str> = rest.split("::").collect();
                if segments.len() < 2 || segments[1] != "crate" {
                    return Err(ScopeError::Malformed(
                        "a module scope is <crate-id>::crate[::<module>...]",
                    ));
                }
                let id: Vec<&str> = segments[0].split('/').collect();
                let package = *id.first().unwrap_or(&"");
                if !is_name(package) || id.len() < 2 {
                    return Err(ScopeError::Malformed(
                        "the module scope needs <package>/<target> before ::crate",
                    ));
                }
                let target = target(&id[1..])?;
                let modules = &segments[2..];
                if modules.len() > MAX_MODULE_DEPTH || !modules.iter().all(|m| is_identifier(m)) {
                    return Err(ScopeError::Malformed(
                        "module names are identifiers, nested at most 16 deep",
                    ));
                }
                Ok(Self::Module {
                    package: package.to_string(),
                    target,
                    modules: modules.iter().map(|m| m.to_string()).collect(),
                })
            }
            other => Err(ScopeError::UnknownForm(other.chars().take(20).collect())),
        }
    }

    /// The scope exactly as PAX is asked for it.
    pub fn render(&self) -> String {
        match self {
            Self::File(path) => format!("file:{path}"),
            Self::Path(path) => format!("path:{path}"),
            Self::Crate { package, target } => match target {
                None => format!("crate:{package}"),
                Some(target) => format!("crate:{package}/{}", target.render()),
            },
            Self::Module {
                package,
                target,
                modules,
            } => {
                let mut text = format!("module:{package}/{}::crate", target.render());
                for module in modules {
                    text.push_str("::");
                    text.push_str(module);
                }
                text
            }
        }
    }

    /// The project-relative path this scope names, if it names one.
    fn path(&self) -> Option<&str> {
        match self {
            Self::File(path) | Self::Path(path) => Some(path),
            _ => None,
        }
    }
}

/// Walks `relative` from `root` one component at a time. A symbolic link on the way is refused
/// whatever it points at, and the resolved target must be inside the canonical root. A component
/// that does not exist ends the walk: there is nothing there to escape through, and PAX will say it
/// was not found.
fn contained(root: &Path, relative: &str) -> Result<(), ScopeError> {
    let canonical_root = std::fs::canonicalize(root).map_err(|_| ScopeError::Unreadable)?;
    let mut at = root.to_path_buf();
    for part in relative.split('/') {
        at.push(part);
        match std::fs::symlink_metadata(&at) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(ScopeError::Symlink(part.to_string()));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) => return Err(ScopeError::Unreadable),
        }
    }
    match std::fs::canonicalize(&at) {
        Ok(resolved) if resolved.starts_with(&canonical_root) => Ok(()),
        Ok(_) => Err(ScopeError::EscapesRoot),
        Err(_) => Err(ScopeError::Unreadable),
    }
}

// ---- PAX's document, parsed strictly ----------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Strength {
    Declared,
    Observed,
    Resolved,
}

impl Strength {
    fn mark(self) -> &'static str {
        match self {
            Self::Observed => "",
            Self::Declared => " [declared]",
            Self::Resolved => " [resolved]",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Relationship {
    ArtifactExists,
    WorkspaceMember,
    DependencyDeclared,
    CrateContains,
    ModuleContains,
    ModuleLocatedAt,
    DeclarationLocatedAt,
    TestDeclared,
}

impl Relationship {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "artifact.exists" => Self::ArtifactExists,
            "workspace.member" => Self::WorkspaceMember,
            "dependency.declared" => Self::DependencyDeclared,
            "crate.contains" => Self::CrateContains,
            "module.contains" => Self::ModuleContains,
            "module.located_at" => Self::ModuleLocatedAt,
            "declaration.located_at" => Self::DeclarationLocatedAt,
            "test.declared" => Self::TestDeclared,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Node {
    kind: String,
    id: String,
    attributes: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Location {
    path: String,
    line: Option<u64>,
}

impl Location {
    fn render(&self) -> String {
        match self.line {
            Some(line) => format!("{}:{line}", self.path),
            None => self.path.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Fact {
    relationship: Relationship,
    subject: Node,
    object: Option<Node>,
    location: Option<Location>,
    strength: Strength,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Diagnostic {
    code: String,
    state: String,
    message: String,
    location: Option<Location>,
}

#[derive(Debug, PartialEq, Eq)]
struct Document {
    partial: bool,
    facts: Vec<Fact>,
    diagnostics: Vec<Diagnostic>,
    files_inspected: u64,
    bytes_read: u64,
    cost_facts: u64,
}

/// An error PAX reported, as a typed document.
#[derive(Debug, PartialEq, Eq)]
struct Refusal {
    code: String,
    limit: Option<String>,
    limit_value: Option<u64>,
}

fn bad(why: impl Into<String>) -> String {
    why.into()
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    let s = value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| bad(format!("`{key}` is missing or not text")))?;
    clean(s)?;
    Ok(s)
}

/// A string Chip will render: bounded and free of control characters.
fn clean(s: &str) -> Result<(), String> {
    if s.len() > MAX_FIELD_BYTES || s.chars().any(char::is_control) {
        return Err(bad("a field is too long or contains a control character"));
    }
    Ok(())
}

fn number(value: &Value, key: &str) -> Result<u64, String> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| bad(format!("`{key}` is missing or not a count")))
}

fn node(value: &Value) -> Result<Node, String> {
    let mut attributes = BTreeMap::new();
    if let Some(map) = value.get("attributes") {
        for (k, v) in map
            .as_object()
            .ok_or_else(|| bad("`attributes` is not an object"))?
        {
            let v = v.as_str().ok_or_else(|| bad("an attribute is not text"))?;
            clean(k)?;
            clean(v)?;
            attributes.insert(k.clone(), v.to_string());
        }
    }
    Ok(Node {
        kind: text(value, "type")?.to_string(),
        id: text(value, "id")?.to_string(),
        attributes,
    })
}

fn location(value: &Value) -> Result<Option<Location>, String> {
    match value {
        Value::Null => Ok(None),
        Value::Object(_) => {
            let line = match value.get("line") {
                None | Some(Value::Null) => None,
                Some(n) => Some(n.as_u64().ok_or_else(|| bad("a line is not a count"))?),
            };
            Ok(Some(Location {
                path: text(value, "path")?.to_string(),
                line,
            }))
        }
        _ => Err(bad("a location is not an object")),
    }
}

/// A path PAX names must be project-relative and lexically plain. Chip also checks it against the
/// filesystem (see [`omit_facts_behind_links`]).
fn relative_artifact(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains(['\\', '\0'])
        && path
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != "..")
}

fn parse_document(stdout: &[u8], requested: &str) -> Result<Document, String> {
    let doc: Value =
        serde_json::from_slice(stdout).map_err(|_| bad("stdout is not one JSON document"))?;
    if !doc.is_object() {
        return Err(bad("the document is not an object"));
    }
    if text(&doc, "schema")? != OBSERVATION_SCHEMA {
        return Err(bad("the schema is not pax.observation.v1"));
    }
    let partial = match text(&doc, "status")? {
        "ok" => false,
        "partial" => true,
        _ => return Err(bad("the status is not ok or partial")),
    };
    // The observation must be of the scope that was asked for, or it is not the answer.
    let scope = doc.get("scope").ok_or_else(|| bad("`scope` is missing"))?;
    let echoed = match scope.get("value").and_then(Value::as_str) {
        Some(value) => format!("{}:{value}", text(scope, "kind")?),
        None => text(scope, "kind")?.to_string(),
    };
    if echoed != requested {
        return Err(bad("the observed scope is not the requested scope"));
    }
    let cost = doc.get("cost").ok_or_else(|| bad("`cost` is missing"))?;
    let (files_inspected, bytes_read, cost_facts) = (
        number(cost, "files_inspected")?,
        number(cost, "bytes_read")?,
        number(cost, "facts")?,
    );
    let mut facts = Vec::new();
    for fact in doc
        .get("facts")
        .and_then(Value::as_array)
        .ok_or_else(|| bad("`facts` is missing or not a list"))?
    {
        let relationship = Relationship::parse(text(fact, "relationship")?)
            .ok_or_else(|| bad("a fact has a relationship pax.observation.v1 does not define"))?;
        let provenance = fact
            .get("provenance")
            .ok_or_else(|| bad("a fact has no provenance"))?;
        let strength = match text(provenance, "strength")? {
            "declared" => Strength::Declared,
            "observed" => Strength::Observed,
            "resolved" => Strength::Resolved,
            _ => {
                return Err(bad(
                    "a fact has a provenance strength this adapter does not accept",
                ));
            }
        };
        text(provenance, "method")?;
        text(provenance, "source")?;
        let object = match fact.get("object") {
            None | Some(Value::Null) => None,
            Some(o) => Some(node(o)?),
        };
        let loc = match fact.get("location") {
            None => None,
            Some(l) => location(l)?,
        };
        if let Some(l) = &loc
            && !relative_artifact(&l.path)
        {
            return Err(bad("a fact names an artifact that is not project-relative"));
        }
        facts.push(Fact {
            relationship,
            subject: node(
                fact.get("subject")
                    .ok_or_else(|| bad("a fact has no subject"))?,
            )?,
            object,
            location: loc,
            strength,
        });
    }
    if facts.len() as u64 > MAX_FACTS {
        return Err(bad("the document holds more facts than the bound allows"));
    }
    let mut diagnostics = Vec::new();
    for d in doc
        .get("diagnostics")
        .and_then(Value::as_array)
        .ok_or_else(|| bad("`diagnostics` is missing or not a list"))?
    {
        let code = text(d, "code")?;
        if code.is_empty() || !code.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
            return Err(bad("a diagnostic code is not a plain code"));
        }
        let state = text(d, "state")?;
        if !["unsupported", "unparseable", "unreadable", "unresolved"].contains(&state) {
            return Err(bad(
                "a diagnostic has a state pax.observation.v1 does not define",
            ));
        }
        let loc = match d.get("location") {
            None => None,
            Some(l) => location(l)?,
        };
        if let Some(l) = &loc
            && !relative_artifact(&l.path)
        {
            return Err(bad(
                "a diagnostic names an artifact that is not project-relative",
            ));
        }
        diagnostics.push(Diagnostic {
            code: code.to_string(),
            state: state.to_string(),
            message: text(d, "message").unwrap_or("").to_string(),
            location: loc,
        });
    }
    // `partial` means at least one diagnostic; an `ok` with diagnostics, or a `partial` without,
    // is not a document Chip can say anything true about.
    if partial != !diagnostics.is_empty() {
        return Err(bad("the status and the diagnostics disagree"));
    }
    Ok(Document {
        partial,
        facts,
        diagnostics,
        files_inspected,
        bytes_read,
        cost_facts,
    })
}

fn parse_refusal(stderr: &[u8]) -> Option<Refusal> {
    let doc: Value = serde_json::from_slice(stderr).ok()?;
    if doc.get("schema")?.as_str()? != OBSERVATION_SCHEMA || doc.get("status")?.as_str()? != "error"
    {
        return None;
    }
    Some(Refusal {
        code: doc.get("code")?.as_str()?.to_string(),
        limit: doc.get("limit").and_then(Value::as_str).map(str::to_string),
        limit_value: doc.get("limit_value").and_then(Value::as_u64),
    })
}

/// Facts about an artifact behind a symbolic link are not shown. A scope that names crates and
/// modules has no path for Chip to check beforehand, so this is where that boundary is held: each
/// distinct artifact a fact is located at is walked as the scope would have been, and a fact located
/// at one reached through a symbolic link (or outside the root) is omitted. The observation is then
/// `partial` and says how many facts were omitted, so what was left out is never mistaken for
/// absent. The model never sees an omitted fact.
///
/// A *diagnostic* is not checked this way. It records something PAX declined to observe (for
/// example a module file behind a symlink that leaves the root); it names the link but claims
/// nothing about what is behind it, and Chip renders only its code, state and location.
///
/// Returns how many facts were omitted.
fn omit_facts_behind_links(root: &Path, doc: &mut Document) -> usize {
    let mut verdict: BTreeMap<String, bool> = BTreeMap::new();
    let before = doc.facts.len();
    doc.facts.retain(|fact| {
        let Some(location) = &fact.location else {
            return true;
        };
        *verdict
            .entry(location.path.clone())
            .or_insert_with(|| contained(root, &location.path).is_ok())
    });
    let omitted = before - doc.facts.len();
    if omitted > 0 {
        doc.partial = true;
        doc.diagnostics.push(Diagnostic {
            code: "facts_omitted_behind_symlink".to_string(),
            state: "unreadable".to_string(),
            message: String::new(),
            location: None,
        });
    }
    omitted
}

// ---- rendering ----------------------------------------------------------------------------------------

fn line(fields: Value) -> String {
    serde_json::to_string(&fields).expect("a JSON value serialises")
}

/// What follows the head line for each state that is not a rendered observation. Chip's words,
/// never PAX's: what is known and what is not.
fn explanation(state: ObservationState) -> &'static str {
    match state {
        ObservationState::InvalidScope => {
            "NOT OBSERVED: PAX found no such scope, or it is not valid. Nothing is known about it from this result."
        }
        ObservationState::LimitExceeded => {
            "NOT OBSERVED: a bound was exceeded, so no facts are given. Absence of a fact here means nothing. Narrow the scope."
        }
        ObservationState::ArtifactUnreadable => {
            "NOT OBSERVED: PAX could not read an artifact it needed. Nothing is known from this result."
        }
        ObservationState::Unsupported => {
            "NOT OBSERVED: PAX does not support this scope or project. Nothing is known from this result."
        }
        ObservationState::Malformed => {
            "NOT AN OBSERVATION: the observer's output was not usable and was discarded. Nothing is known from this result."
        }
        ObservationState::Complete | ObservationState::Partial => "",
    }
}

fn head(state: ObservationState, scope: &str, pax: &str, extra: Value) -> String {
    let mut fields = serde_json::json!({
        "capability": PROJECT_OBSERVE_CAPABILITY,
        "state": state.name(),
        "scope": scope,
        "schema": OBSERVATION_SCHEMA,
        "pax": pax,
    });
    if let (Some(base), Some(more)) = (fields.as_object_mut(), extra.as_object()) {
        base.extend(more.clone());
    }
    line(fields)
}

/// What the model is shown, and whether all of what PAX observed is in it.
#[derive(Debug, PartialEq, Eq)]
struct Rendered {
    text: String,
    state: ObservationState,
}

fn short_module(module: &str) -> &str {
    // `pkg/lib::crate::a::b` -> `a::b`; the crate root is empty.
    module
        .split_once("::")
        .map(|(_, rest)| rest.strip_prefix("crate").unwrap_or(rest))
        .map(|rest| rest.strip_prefix("::").unwrap_or(rest))
        .unwrap_or(module)
}

fn render(doc: &Document, scope: &str, pax: &str, omitted: usize) -> Rendered {
    // Rows per section, in the order they are shown; diagnostics first, so they are never the part
    // that does not fit.
    let mut sections: Vec<(&'static str, Vec<String>)> = Vec::new();
    let mut section = |title: &'static str, rows: Vec<String>| {
        if !rows.is_empty() {
            sections.push((title, rows));
        }
    };

    let mut diagnostics: Vec<String> = doc
        .diagnostics
        .iter()
        .map(|d| {
            let at = d
                .location
                .as_ref()
                .map(|l| format!(" {}", l.render()))
                .unwrap_or_default();
            format!("{} ({}){at}", d.code, d.state)
        })
        .collect();
    diagnostics.sort();
    section(
        "--- NOT ESTABLISHED (the observation is incomplete) ---",
        diagnostics,
    );

    let facts_of = |r: Relationship| doc.facts.iter().filter(move |f| f.relationship == r);

    // Declarations, grouped by file, in source order.
    let mut decls: Vec<&Fact> = facts_of(Relationship::DeclarationLocatedAt).collect();
    decls.sort_by(|a, b| {
        let key = |f: &&Fact| {
            f.location
                .as_ref()
                .map(|l| (l.path.clone(), l.line.unwrap_or(0)))
        };
        key(a).cmp(&key(b)).then(a.subject.id.cmp(&b.subject.id))
    });
    let mut rows = Vec::new();
    let mut current: Option<&str> = None;
    for f in decls {
        let (path, line) = f
            .location
            .as_ref()
            .map(|l| (l.path.as_str(), l.line.unwrap_or(0)))
            .unwrap_or(("?", 0));
        if current != Some(path) {
            rows.push(path.to_string());
            current = Some(path);
        }
        let a = &f.subject.attributes;
        let name = f.subject.id.rsplit("::").next().unwrap_or(&f.subject.id);
        let module = a.get("module").map(|m| short_module(m)).unwrap_or("");
        let within = if module.is_empty() {
            String::new()
        } else {
            format!(" in {module}")
        };
        rows.push(format!(
            "  :{line} {} {} {name}{within}{}",
            a.get("visibility").map(String::as_str).unwrap_or("?"),
            a.get("kind").map(String::as_str).unwrap_or("?"),
            f.strength.mark()
        ));
    }
    section("--- declarations (name:line) ---", rows);

    let mut tests: Vec<&Fact> = facts_of(Relationship::TestDeclared).collect();
    tests.sort_by(|a, b| {
        let key = |f: &&Fact| {
            f.location
                .as_ref()
                .map(|l| (l.path.clone(), l.line.unwrap_or(0)))
        };
        key(a).cmp(&key(b))
    });
    let rows = tests
        .iter()
        .map(|f| {
            let at = f
                .location
                .as_ref()
                .map(Location::render)
                .unwrap_or_default();
            let name = f.subject.id.rsplit("::").next().unwrap_or(&f.subject.id);
            let attr = f
                .subject
                .attributes
                .get("attribute")
                .map(|a| format!(" ({a})"))
                .unwrap_or_default();
            format!("{at} {name}{attr}{}", f.strength.mark())
        })
        .collect();
    section(
        "--- tests declared (an attribute is present; not what is proven) ---",
        rows,
    );

    let mut rows: Vec<String> = facts_of(Relationship::ModuleLocatedAt)
        .map(|f| {
            let at = f
                .location
                .as_ref()
                .map(Location::render)
                .unwrap_or_default();
            format!("{} at {at}{}", f.subject.id, f.strength.mark())
        })
        .chain(facts_of(Relationship::ModuleContains).map(|f| {
            let child = f.object.as_ref().map(|o| o.id.as_str()).unwrap_or("?");
            format!("{} contains {child}{}", f.subject.id, f.strength.mark())
        }))
        .collect();
    rows.sort();
    section("--- modules ---", rows);

    let mut rows: Vec<String> = facts_of(Relationship::CrateContains)
        .map(|f| {
            let root = f.object.as_ref().map(|o| o.id.as_str()).unwrap_or("?");
            let at = f
                .location
                .as_ref()
                .map(Location::render)
                .unwrap_or_default();
            format!(
                "{} root module {root} at {at}{}",
                f.subject.id,
                f.strength.mark()
            )
        })
        .collect();
    rows.sort();
    section("--- crates ---", rows);

    let mut rows: Vec<String> = facts_of(Relationship::WorkspaceMember)
        .map(|f| {
            let member = f.object.as_ref().map(|o| o.id.as_str()).unwrap_or("?");
            let at = f
                .location
                .as_ref()
                .map(Location::render)
                .unwrap_or_default();
            format!("{member} ({at}){}", f.strength.mark())
        })
        .collect();
    rows.sort();
    section("--- workspace members ---", rows);

    let mut rows: Vec<String> = facts_of(Relationship::DependencyDeclared)
        .map(|f| {
            let dep = f.object.as_ref();
            let name = dep.map(|o| o.id.as_str()).unwrap_or("?");
            let a = dep.map(|o| &o.attributes);
            let get = |k: &str| a.and_then(|a| a.get(k)).map(String::as_str).unwrap_or("?");
            format!(
                "{name} {} ({}){}",
                get("specifier"),
                get("kind"),
                f.strength.mark()
            )
        })
        .collect();
    rows.sort();
    section(
        "--- dependencies (declared; aggregated across workspace members, not attributed to a package) ---",
        rows,
    );

    let mut rows: Vec<String> = facts_of(Relationship::ArtifactExists)
        .map(|f| format!("{} ({}){}", f.subject.id, f.subject.kind, f.strength.mark()))
        .collect();
    rows.sort();
    section("--- artifacts that exist ---", rows);

    // Assemble within the bound. What does not fit is counted, never silently dropped.
    let total_rows: usize = sections.iter().map(|(_, r)| r.len()).sum();
    let mut body = String::new();
    let mut shown = 0usize;
    let mut not_shown: Vec<(&str, usize)> = Vec::new();
    let reserve = 1600; // the head, the preamble and the trailer: the bound is on the whole result
    let mut full = false;
    for (title, rows) in &sections {
        let mut included = 0usize;
        if !full {
            let header = format!("{title}\n");
            if body.len() + header.len() + reserve <= MAX_RENDERED_BYTES {
                body.push_str(&header);
                for row in rows {
                    let row = format!("{row}\n");
                    if body.len() + row.len() + reserve > MAX_RENDERED_BYTES {
                        full = true;
                        break;
                    }
                    body.push_str(&row);
                    included += 1;
                }
            } else {
                full = true;
            }
        }
        shown += included;
        if included < rows.len() {
            not_shown.push((title, rows.len() - included));
        }
    }
    let truncated = !not_shown.is_empty();
    if truncated {
        body.push_str(
            "--- NOT SHOWN (render bound; these were observed but are not in this result) ---\n",
        );
        for (title, n) in &not_shown {
            let name = title.trim_matches('-').trim();
            let name = name.split(" (").next().unwrap_or(name);
            body.push_str(&format!("{n} more rows of: {name}\n"));
        }
    }
    let state = if doc.partial || truncated {
        ObservationState::Partial
    } else {
        ObservationState::Complete
    };
    let mut reasons = Vec::new();
    if doc
        .diagnostics
        .iter()
        .any(|d| d.code != "facts_omitted_behind_symlink")
    {
        reasons.push("pax_diagnostics");
    }
    if omitted > 0 {
        reasons.push("facts_behind_symlink");
    }
    if truncated {
        reasons.push("render_bound");
    }
    let mut text = head(
        state,
        scope,
        pax,
        serde_json::json!({
            "facts": doc.facts.len(),
            "rows": total_rows,
            "rows_shown": shown,
            "files_inspected": doc.files_inspected,
            "bytes_read": doc.bytes_read,
            "omitted_facts": omitted,
            "reasons": reasons,
        }),
    );
    text.push('\n');
    text.push_str(&format!(
        "OBSERVATION {PROJECT_OBSERVE_CAPABILITY}\nscope: {scope}\nstatus: {}\n",
        state.name()
    ));
    text.push_str(&format!(
        "source: pax {pax}, {OBSERVATION_SCHEMA}; {} facts, {} files inspected, {} bytes read\n",
        doc.cost_facts, doc.files_inspected, doc.bytes_read
    ));
    text.push_str("provenance: unmarked = observed (PAX read the artifact); [declared] = an artifact states it, unchecked; [resolved] = a resolver established it\n");
    if state == ObservationState::Partial {
        text.push_str("OBSERVATION INCOMPLETE: a fact that is not listed is NOT KNOWN ABSENT; it was not observed.\n");
    }
    text.push_str(&body);
    Rendered { text, state }
}

/// A `file:` scope names one artifact. If PAX could not read it, nothing was observed about the
/// scope, which is `artifact_unreadable`, not a partial observation of something.
fn target_unreadable(doc: &Document, scope: &Scope) -> bool {
    let Scope::File(path) = scope else {
        return false;
    };
    doc.diagnostics.iter().any(|d| {
        d.code == "artifact_unreadable" && d.location.as_ref().is_some_and(|l| &l.path == path)
    })
}

fn refusal_state(refusal: &Refusal) -> ObservationState {
    match refusal.code.as_str() {
        "invalid_scope" | "scope_not_found" => ObservationState::InvalidScope,
        "unsupported_scope" | "unsupported_project" => ObservationState::Unsupported,
        "limit_exceeded" => ObservationState::LimitExceeded,
        "artifact_unreadable" => ObservationState::ArtifactUnreadable,
        // `invalid_limit` means Chip's own bounds were refused: a mismatch at the boundary, not an
        // answer. An unknown code is not one this adapter can say anything true about.
        _ => ObservationState::Malformed,
    }
}

fn refusal_text(refusal: &Refusal, scope: &str, pax: &str) -> Rendered {
    let state = refusal_state(refusal);
    let known = |s: &str| {
        [
            "scope_not_found",
            "invalid_scope",
            "unsupported_scope",
            "unsupported_project",
            "limit_exceeded",
            "artifact_unreadable",
        ]
        .contains(&s)
    };
    let limit = refusal
        .limit
        .as_deref()
        .filter(|l| ["max_files", "max_bytes", "max_facts"].contains(l));
    let extra = serde_json::json!({
        "code": if known(&refusal.code) { Value::from(refusal.code.clone()) } else { Value::Null },
        "limit": limit,
        "limit_value": if limit.is_some() { refusal.limit_value.map(Value::from).unwrap_or(Value::Null) } else { Value::Null },
    });
    let mut text = head(state, scope, pax, extra);
    text.push('\n');
    text.push_str(explanation(state));
    Rendered { text, state }
}

fn malformed_text(scope: &str, pax: &str, reason: &str) -> Rendered {
    let mut text = head(
        ObservationState::Malformed,
        scope,
        pax,
        serde_json::json!({ "reason": reason }),
    );
    text.push('\n');
    text.push_str(explanation(ObservationState::Malformed));
    Rendered {
        text,
        state: ObservationState::Malformed,
    }
}

// ---- the capability ---------------------------------------------------------------------------------------

fn scope_input(inputs: &BTreeMap<String, InputValue>) -> Result<Scope, CapabilityError> {
    for name in inputs.keys() {
        if name != "scope" {
            return Err(CapabilityError::InvalidInput(format!(
                "capability does not accept input '{name}'"
            )));
        }
    }
    match inputs.get("scope") {
        Some(InputValue::Text(text)) => {
            Scope::parse(text).map_err(|e| CapabilityError::InvalidInput(e.to_string()))
        }
        Some(_) => Err(CapabilityError::InvalidInput(
            "input 'scope' must be text".into(),
        )),
        None => Err(CapabilityError::InvalidInput(
            "missing required input 'scope'".into(),
        )),
    }
}

/// The project-structure observation capability over one project directory.
#[derive(Debug, Clone)]
pub struct PaxObserve {
    pax: PaxExecutor,
}

impl PaxObserve {
    /// Shares `pax`'s verification, so PAX's identity is probed at most once per work.
    pub fn new(pax: PaxExecutor) -> Self {
        Self { pax }
    }

    /// The arguments PAX is run with. Fixed except for the validated scope, which is one discrete
    /// argument and is never interpreted by a shell.
    pub fn invocation(&self, scope: &Scope) -> Vec<std::ffi::OsString> {
        vec![
            "--dir".into(),
            self.pax.work_directory().as_os_str().to_owned(),
            "--json".into(),
            "observe".into(),
            "--scope".into(),
            scope.render().into(),
            "--max-files".into(),
            MAX_FILES.to_string().into(),
            "--max-bytes".into(),
            MAX_BYTES.to_string().into(),
            "--max-facts".into(),
            MAX_FACTS.to_string().into(),
        ]
    }

    /// Validates a scope against the grammar and the project root. Touches no process.
    fn check_scope(&self, scope: &Scope) -> Result<(), CapabilityError> {
        if let Some(path) = scope.path() {
            contained(self.pax.work_directory(), path)
                .map_err(|e| CapabilityError::InvalidInput(e.to_string()))?;
        }
        Ok(())
    }

    /// PAX, verified, and new enough to observe. The version is the contract.
    async fn verified(&self) -> Result<String, String> {
        let pax = self.pax.resolve().await.map_err(|why| why.to_string())?;
        let found = Version::parse(&pax.version);
        if !found
            .as_ref()
            .is_some_and(|v| v.at_least(MIN_OBSERVE_PAX_VERSION))
        {
            let (a, b, c) = MIN_OBSERVE_PAX_VERSION;
            return Err(format!(
                "PAX observation requires >= {a}.{b}.{c}; found {}",
                pax.version
            ));
        }
        Ok(pax.version)
    }

    /// A bounded diagnostic from PAX's stderr: never evidence, never a host path.
    fn diagnostic(&self, stderr: &[u8]) -> String {
        let mut text = String::from_utf8_lossy(stderr).into_owned();
        let wd = self.pax.work_directory();
        let mut spellings = vec![wd.to_string_lossy().into_owned()];
        if let Ok(real) = std::fs::canonicalize(wd) {
            spellings.push(real.to_string_lossy().into_owned());
        }
        for s in spellings.into_iter().filter(|s| s.len() > 1) {
            text = text.replace(&s, "<project>");
        }
        let text: String = text
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        text.chars().take(MAX_DIAGNOSTIC_BYTES).collect()
    }
}

#[async_trait::async_trait]
impl CapabilityProvider for PaxObserve {
    async fn capabilities(&self) -> Result<Vec<CapabilityDescriptor>, CapabilityError> {
        let mut d = CapabilityDescriptor::new(
            CapabilityId::new(PROJECT_OBSERVE_CAPABILITY)?,
            "Observe project structure",
            "Project structure via PAX. scope: crate:<pkg> | module:<pkg>/lib::crate[::<mod>] | file:<path>.rs | path:<dir>. Facts only, not relevance. Use narrow scopes.",
        )
        // Source changes between requests, so the structure is observed again every time.
        .without_evidence_reuse();
        d.inputs = vec![CapabilityInput {
            name: "scope".to_string(),
            description: "crate:<package> or crate:<package>/lib, module:<package>/lib::crate[::<mod>], file:<path.rs> or path:<prefix>"
                .to_string(),
            required: true,
        }];
        Ok(vec![d])
    }

    /// Cheap and process-free, like `pax.test`: a `pax` file in place is "available". That it is
    /// PAX, and a release that can observe, is verified when a request is validated.
    async fn availability(&self, id: &CapabilityId) -> CapabilityAvailability {
        if id.as_str() != PROJECT_OBSERVE_CAPABILITY {
            return CapabilityAvailability::Unavailable("capability is not provided".into());
        }
        match self.pax.locate() {
            Ok(_) => CapabilityAvailability::Available,
            Err(why) => CapabilityAvailability::Unavailable(why.to_string()),
        }
    }

    /// The grammar and the containment first, with no process started. Only a request that passes
    /// reaches the version check, and only then is anything run.
    async fn validate_inputs(
        &self,
        id: &CapabilityId,
        inputs: &BTreeMap<String, InputValue>,
    ) -> Result<(), CapabilityError> {
        if id.as_str() != PROJECT_OBSERVE_CAPABILITY {
            return Err(CapabilityError::Unknown(id.to_string()));
        }
        let scope = scope_input(inputs)?;
        self.check_scope(&scope)?;
        self.verified()
            .await
            .map_err(CapabilityError::Unavailable)?;
        Ok(())
    }
}

/// Reads at most `cap + 1` bytes, so a stream that is over the bound is known to be.
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(stream: Option<R>, cap: usize) -> Vec<u8> {
    let mut out = Vec::new();
    if let Some(stream) = stream {
        let _ = stream.take(cap as u64 + 1).read_to_end(&mut out).await;
    }
    out
}

#[async_trait::async_trait]
impl Executor for PaxObserve {
    async fn execute(&self, request: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        if request.intent != PROJECT_OBSERVE_CAPABILITY {
            return Err(ExecutionError::InvalidRequest(format!(
                "'{}' is not a capability this executor provides",
                request.intent
            )));
        }
        let invalid = |why: CapabilityError| ExecutionError::InvalidRequest(why.to_string());
        let scope = scope_input(&request.inputs).map_err(invalid)?;
        self.check_scope(&scope).map_err(invalid)?;
        let pax_version = self
            .verified()
            .await
            .map_err(ExecutionError::ExecutorUnavailable)?;
        let pax = self
            .pax
            .resolve()
            .await
            .map_err(|why| ExecutionError::ExecutorUnavailable(why.to_string()))?;
        let rendered_scope = scope.render();

        let mut child = Command::new(&pax.path)
            .args(self.invocation(&scope))
            .current_dir(self.pax.work_directory())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| ExecutionError::ExecutorUnavailable("pax could not be started".into()))?;
        let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
        let run = async {
            let (out, err) = tokio::join!(
                read_capped(stdout, MAX_OUTPUT_BYTES),
                read_capped(stderr, MAX_STDERR_BYTES)
            );
            let over = out.len() > MAX_OUTPUT_BYTES;
            if over {
                let _ = child.kill().await;
            }
            let status = child.wait().await;
            (out, err, over, status)
        };
        let (out, err, over, status) = match tokio::time::timeout(TIMEOUT, run).await {
            Ok(done) => done,
            Err(_) => {
                return Err(ExecutionError::ExecutionFailed(
                    "pax did not finish within the time limit".into(),
                ));
            }
        };

        let rendered = if over {
            // Chip's own bound on what it will hold: a bounded result, not a truncated document.
            Rendered {
                text: {
                    let mut t = head(
                        ObservationState::LimitExceeded,
                        &rendered_scope,
                        &pax_version,
                        serde_json::json!({"code": null, "limit": "output_bytes", "limit_value": MAX_OUTPUT_BYTES}),
                    );
                    t.push('\n');
                    t.push_str(explanation(ObservationState::LimitExceeded));
                    t
                },
                state: ObservationState::LimitExceeded,
            }
        } else {
            let code = status
                .map_err(|_| ExecutionError::ExecutionFailed("pax could not be waited on".into()))?
                .code();
            match code {
                Some(0) => match parse_document(&out, &rendered_scope) {
                    Ok(mut doc) => {
                        let omitted = omit_facts_behind_links(self.pax.work_directory(), &mut doc);
                        if target_unreadable(&doc, &scope) {
                            refusal_text(
                                &Refusal {
                                    code: "artifact_unreadable".into(),
                                    limit: None,
                                    limit_value: None,
                                },
                                &rendered_scope,
                                &pax_version,
                            )
                        } else {
                            render(&doc, &rendered_scope, &pax_version, omitted)
                        }
                    }
                    Err(why) => malformed_text(&rendered_scope, &pax_version, &why),
                },
                _ => match parse_refusal(&err) {
                    Some(refusal) => refusal_text(&refusal, &rendered_scope, &pax_version),
                    // No typed answer: PAX failed. That is not an observation, empty or otherwise.
                    None => {
                        return Err(ExecutionError::ExecutionFailed(format!(
                            "pax exited {} without a pax.observation.v1 error; diagnostic (not evidence): {}",
                            code.map_or("by signal".to_string(), |c| c.to_string()),
                            self.diagnostic(&err)
                        )));
                    }
                },
            }
        };
        Ok(match rendered.state {
            // The execution ran and produced an observation; whether it is complete is in the text.
            ObservationState::Complete | ObservationState::Partial => {
                ExecutionResult::success(request.id, rendered.text)
            }
            _ => ExecutionResult::failure(request.id, rendered.text),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(text: &str) -> Scope {
        Scope::parse(text).unwrap_or_else(|e| panic!("{text}: {e}"))
    }

    #[test]
    fn the_pinned_release_and_the_minimum_agree() {
        let pinned = Version::parse(PINNED_PAX.version).unwrap();
        assert!(pinned.at_least(MIN_OBSERVE_PAX_VERSION));
        assert_eq!(
            (pinned.major, pinned.minor, pinned.patch),
            MIN_OBSERVE_PAX_VERSION,
            "the minimum is the release the observation contract was verified against"
        );
        assert_eq!(PINNED_PAX.tag, format!("v{}", PINNED_PAX.version));
        assert_eq!(PINNED_PAX.commit.len(), 40);
        assert!(PINNED_PAX.commit.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn every_documented_scope_form_is_accepted_and_rendered_canonically() {
        for (text, expected) in [
            ("file:src/lib.rs", Scope::File("src/lib.rs".into())),
            (
                "path:crates/fx-core/src",
                Scope::Path("crates/fx-core/src".into()),
            ),
            ("path:src", Scope::Path("src".into())),
            (
                "crate:fx-core",
                Scope::Crate {
                    package: "fx-core".into(),
                    target: None,
                },
            ),
            (
                "crate:fx-core/lib",
                Scope::Crate {
                    package: "fx-core".into(),
                    target: Some(Target::Lib),
                },
            ),
            (
                "crate:app/bin/tool",
                Scope::Crate {
                    package: "app".into(),
                    target: Some(Target::Other {
                        kind: "bin".into(),
                        name: "tool".into(),
                    }),
                },
            ),
            (
                "crate:a/test/it",
                Scope::Crate {
                    package: "a".into(),
                    target: Some(Target::Other {
                        kind: "test".into(),
                        name: "it".into(),
                    }),
                },
            ),
            (
                "module:fx-core/lib::crate",
                Scope::Module {
                    package: "fx-core".into(),
                    target: Target::Lib,
                    modules: vec![],
                },
            ),
            (
                "module:fx-core/lib::crate::a::b",
                Scope::Module {
                    package: "fx-core".into(),
                    target: Target::Lib,
                    modules: vec!["a".into(), "b".into()],
                },
            ),
            (
                "module:app/bin/tool::crate::cli",
                Scope::Module {
                    package: "app".into(),
                    target: Target::Other {
                        kind: "bin".into(),
                        name: "tool".into(),
                    },
                    modules: vec!["cli".into()],
                },
            ),
        ] {
            let parsed = scope(text);
            assert_eq!(parsed, expected, "{text}");
            assert_eq!(parsed.render(), text, "{text} is passed to PAX as written");
        }
    }

    #[test]
    fn everything_else_fails_closed_before_anything_runs() {
        for (what, text) in [
            ("empty", ""),
            ("no form", "src/lib.rs"),
            ("whole project", "project"),
            ("project form", "project:."),
            ("unknown form", "symbol:foo"),
            ("a hint at relevance", "relevant:goal"),
            ("a space", "file:src/my lib.rs"),
            ("a newline", "file:src/lib.rs\n--max-files 99999"),
            ("an option smuggled in", "crate:fx --max-files"),
            ("a NUL", "file:src/lib.rs\0"),
            ("unicode", "file:src/é.rs"),
            ("absolute file", "file:/etc/passwd.rs"),
            ("absolute path", "path:/etc"),
            ("empty path", "path:"),
            ("empty file", "file:"),
            ("traversal file", "file:../outside/x.rs"),
            ("traversal path", "path:src/../../x"),
            ("dot component", "path:src/./x"),
            ("empty component", "path:src//x"),
            ("a non-rust file", "file:Cargo.toml"),
            ("a reserved directory", "path:.git"),
            ("a reserved file", "file:src/.env.rs"),
            ("a trailing slash", "path:src/"),
            ("empty package", "crate:"),
            ("a path as a package", "crate:../x"),
            ("an unknown target kind", "crate:a/plugin/x"),
            ("a target with no name", "crate:a/bin"),
            ("lib with a name", "crate:a/lib/x"),
            ("too many parts", "crate:a/bin/x/y"),
            ("module without crate", "module:a/lib::a"),
            ("module without a target", "module:a::crate"),
            ("module with a bad identifier", "module:a/lib::crate::1x"),
            ("module with an empty segment", "module:a/lib::crate::::x"),
            (
                "module nested too deep",
                &format!("module:a/lib::crate{}", "::m".repeat(17)),
            ),
            ("too long", &format!("path:{}", "a".repeat(MAX_SCOPE_BYTES))),
        ] {
            assert!(Scope::parse(text).is_err(), "{what} was accepted: {text:?}");
        }
    }

    fn tree(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("chip-observe-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("project/src")).unwrap();
        std::fs::create_dir_all(dir.join("outside")).unwrap();
        std::fs::write(dir.join("outside/secret.rs"), "pub fn secret() {}").unwrap();
        std::fs::write(dir.join("project/src/lib.rs"), "pub fn f() {}").unwrap();
        dir
    }

    #[cfg(unix)]
    #[test]
    fn containment_refuses_every_symlink_and_every_path_that_resolves_outside() {
        let dir = tree("contain");
        let project = dir.join("project");
        let outside = dir.join("outside");
        std::os::unix::fs::symlink(outside.join("secret.rs"), project.join("src/linked.rs"))
            .unwrap();
        std::os::unix::fs::symlink(&outside, project.join("src/external")).unwrap();
        std::os::unix::fs::symlink(&outside, project.join("link")).unwrap();
        // A symlink that stays inside is refused too: Chip's project capabilities never read through one.
        std::os::unix::fs::symlink(project.join("src/lib.rs"), project.join("src/inside.rs"))
            .unwrap();
        assert_eq!(contained(&project, "src/lib.rs"), Ok(()));
        assert_eq!(contained(&project, "src"), Ok(()));
        assert_eq!(
            contained(&project, "src/not-yet-written.rs"),
            Ok(()),
            "nothing there to escape"
        );
        for (what, path, expected) in [
            (
                "a file symlink to outside",
                "src/linked.rs",
                ScopeError::Symlink("linked.rs".into()),
            ),
            (
                "a directory symlink to outside",
                "src/external",
                ScopeError::Symlink("external".into()),
            ),
            (
                "a symlinked path component",
                "link/secret.rs",
                ScopeError::Symlink("link".into()),
            ),
            (
                "a symlinked path component, deeper",
                "src/external/secret.rs",
                ScopeError::Symlink("external".into()),
            ),
            (
                "a symlink that stays inside",
                "src/inside.rs",
                ScopeError::Symlink("inside.rs".into()),
            ),
        ] {
            assert_eq!(contained(&project, path), Err(expected), "{what}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_rejected_scope_is_an_invalid_request_and_starts_no_process() {
        let dir = tree("noproc");
        let project = dir.join("project");
        std::os::unix::fs::symlink(dir.join("outside/secret.rs"), project.join("src/linked.rs"))
            .unwrap();
        // A `pax` that records being started: it must never be.
        let calls = dir.join("calls");
        let pax = dir.join("pax");
        std::fs::write(
            &pax,
            format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\necho 'pax 0.4.1'\n",
                calls.display()
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&pax, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let observe = PaxObserve::new(PaxExecutor::new(&project).with_binary(&pax));
        let id = CapabilityId::new(PROJECT_OBSERVE_CAPABILITY).unwrap();
        for scope in [
            "file:src/linked.rs",
            "file:/etc/passwd.rs",
            "file:../outside/secret.rs",
            "path:src/../../outside",
            "bogus",
            "",
        ] {
            let mut inputs = BTreeMap::new();
            inputs.insert("scope".to_string(), InputValue::Text(scope.into()));
            assert!(
                matches!(
                    observe.validate_inputs(&id, &inputs).await,
                    Err(CapabilityError::InvalidInput(_))
                ),
                "{scope:?} was accepted"
            );
            let r = observe
                .execute(
                    chip_core::ExecutionRequest::new(
                        chip_core::ExecutionId::new("x"),
                        PROJECT_OBSERVE_CAPABILITY,
                    )
                    .with_inputs(inputs),
                )
                .await;
            assert!(
                matches!(r, Err(ExecutionError::InvalidRequest(_))),
                "{scope:?}: {r:?}"
            );
        }
        assert!(!calls.exists(), "PAX was started for a rejected request");
    }

    // ---- documents ----

    fn fact(
        rel: &str,
        subject: Value,
        object: Value,
        location: Value,
        strength: &str,
        method: &str,
    ) -> Value {
        serde_json::json!({
            "relationship": rel, "subject": subject, "object": object, "location": location,
            "provenance": {"strength": strength, "method": method, "source": "x"}
        })
    }

    fn doc(status: &str, facts: Vec<Value>, diagnostics: Vec<Value>) -> Value {
        serde_json::json!({
            "schema": "pax.observation.v1", "status": status,
            "project": {"root": "/Users/someone/secret-project", "name": "p"},
            "scope": {"kind": "crate", "value": "fx"},
            "observed_at": 1,
            "tool": {"name": "pax", "version": "0.4.1", "parser": "syn 2 (full)"},
            "limits": {"max_files": 50, "max_bytes": 1048576, "max_facts": 600},
            "cost": {"entries_listed": 3, "files_inspected": 2, "files_parsed": 2, "bytes_read": 100, "facts": facts.len(), "elapsed_ms": 1},
            "facts": facts, "diagnostics": diagnostics
        })
    }

    fn decl(name: &str, kind: &str, vis: &str, module: &str, path: &str, line: u64) -> Value {
        fact(
            "declaration.located_at",
            serde_json::json!({"type":"declaration","id":format!("{module}::{name}"),"attributes":{"kind":kind,"visibility":vis,"module":module}}),
            Value::Null,
            serde_json::json!({"path":path,"line":line}),
            "observed",
            "syn.parse_file",
        )
    }

    fn sample_facts() -> Vec<Value> {
        vec![
            decl("Alpha", "struct", "pub", "fx/lib::crate", "src/lib.rs", 4),
            decl("beta", "fn", "pub", "fx/lib::crate::util", "src/util.rs", 9),
            fact(
                "test.declared",
                serde_json::json!({"type":"test","id":"fx/lib::crate::util::checks","attributes":{"attribute":"test","module":"fx/lib::crate::util"}}),
                Value::Null,
                serde_json::json!({"path":"src/util.rs","line":20}),
                "observed",
                "syn.parse_file",
            ),
            fact(
                "module.contains",
                serde_json::json!({"type":"module","id":"fx/lib::crate"}),
                serde_json::json!({"type":"module","id":"fx/lib::crate::missing"}),
                Value::Null,
                "declared",
                "syn.parse_file",
            ),
            fact(
                "workspace.member",
                serde_json::json!({"type":"workspace","id":"."}),
                serde_json::json!({"type":"package","id":"fx"}),
                serde_json::json!({"path":"Cargo.toml"}),
                "resolved",
                "cargo.metadata",
            ),
            fact(
                "dependency.declared",
                serde_json::json!({"type":"workspace","id":"."}),
                serde_json::json!({"type":"dependency","id":"serde","attributes":{"kind":"dependency","specifier":"^1"}}),
                Value::Null,
                "declared",
                "cargo.metadata",
            ),
            fact(
                "artifact.exists",
                serde_json::json!({"type":"manifest","id":"Cargo.toml"}),
                Value::Null,
                serde_json::json!({"path":"Cargo.toml"}),
                "observed",
                "fs.stat",
            ),
        ]
    }

    fn parsed(v: &Value) -> Result<Document, String> {
        parse_document(v.to_string().as_bytes(), "crate:fx")
    }

    #[test]
    fn a_complete_observation_is_complete_and_keeps_every_fact_and_its_strength() {
        let d = parsed(&doc("ok", sample_facts(), vec![])).unwrap();
        let r = render(&d, "crate:fx", "0.4.1", 0);
        assert_eq!(r.state, ObservationState::Complete);
        assert_eq!(parse_state(&r.text), Some(ObservationState::Complete));
        let t = &r.text;
        for expected in [
            "src/lib.rs\n  :4 pub struct Alpha",
            "src/util.rs\n  :9 pub fn beta in util",
            "src/util.rs:20 checks (test)",
            "fx/lib::crate contains fx/lib::crate::missing [declared]",
            "fx (Cargo.toml) [resolved]",
            "serde ^1 (dependency) [declared]",
            "Cargo.toml (manifest)",
        ] {
            assert!(t.contains(expected), "missing {expected:?} in:\n{t}");
        }
        assert!(t.contains("aggregated across workspace members, not attributed to a package"));
        assert!(!t.contains("INCOMPLETE") && !t.contains("NOT SHOWN"));
        assert!(
            !t.contains("Alpha [declared]") && !t.contains("Alpha [resolved]"),
            "an observed fact is not upgraded or downgraded"
        );
    }

    #[test]
    fn a_partial_observation_is_never_rendered_as_complete() {
        let d = parsed(&doc(
            "partial",
            sample_facts(),
            vec![serde_json::json!({"code":"syntax_error","state":"unparseable","message":"x","location":{"path":"src/broken.rs","line":1}})],
        ))
        .unwrap();
        let r = render(&d, "crate:fx", "0.4.1", 0);
        assert_eq!(r.state, ObservationState::Partial);
        assert_eq!(parse_state(&r.text), Some(ObservationState::Partial));
        assert!(r.text.contains("status: partial"));
        assert!(r.text.contains("NOT KNOWN ABSENT"));
        assert!(
            r.text
                .contains("syntax_error (unparseable) src/broken.rs:1")
        );
        assert!(!r.text.contains("status: complete"));
        // The diagnostics come first, so they are never what does not fit.
        assert!(r.text.find("NOT ESTABLISHED").unwrap() < r.text.find("--- declarations").unwrap());
    }

    #[test]
    fn rendering_stays_within_its_bound_and_says_what_it_left_out() {
        let facts: Vec<Value> = (0..MAX_FACTS as usize)
            .map(|i| {
                decl(
                    &format!("a_deliberately_long_declaration_name_for_the_bound_{i:04}"),
                    "fn",
                    "pub",
                    "fx/lib::crate",
                    "src/lib.rs",
                    i as u64 + 1,
                )
            })
            .collect();
        let d = parsed(&doc("ok", facts, vec![])).unwrap();
        let r = render(&d, "crate:fx", "0.4.1", 0);
        assert!(r.text.len() <= MAX_RENDERED_BYTES, "{} bytes", r.text.len());
        assert_eq!(
            r.state,
            ObservationState::Partial,
            "what does not fit is not complete"
        );
        assert!(r.text.contains("NOT SHOWN"));
        assert!(r.text.contains("NOT KNOWN ABSENT"));
        let head: Value = serde_json::from_str(r.text.lines().next().unwrap()).unwrap();
        assert_eq!(head["facts"], MAX_FACTS);
        assert!(head["rows_shown"].as_u64().unwrap() < head["rows"].as_u64().unwrap());
        assert_eq!(head["reasons"], serde_json::json!(["render_bound"]));
    }

    #[test]
    fn rendering_is_deterministic_and_adds_no_interpretation() {
        let d = parsed(&doc("ok", sample_facts(), vec![])).unwrap();
        let a = render(&d, "crate:fx", "0.4.1", 0).text;
        assert_eq!(a, render(&d, "crate:fx", "0.4.1", 0).text);
        let lower = a.to_lowercase();
        for word in [
            "relevant",
            "recommend",
            "likely",
            "should",
            "safe to",
            "impact",
            "fix",
            "goal",
            "responsible",
        ] {
            assert!(!lower.contains(word), "the rendering says {word:?}:\n{a}");
        }
        assert!(!a.contains("/Users/"), "no host path");
        assert!(
            !a.contains("secret-project"),
            "the project root is not passed on"
        );
        assert!(!a.contains("observed_at"));
    }

    #[test]
    fn a_document_that_is_not_exactly_what_is_understood_is_malformed() {
        let ok = doc("ok", sample_facts(), vec![]);
        let with = |f: &dyn Fn(&mut Value)| {
            let mut d = ok.clone();
            f(&mut d);
            parsed(&d)
        };
        for (what, result) in [
            (
                "another schema",
                with(&|d| d["schema"] = "pax.observation.v2".into()),
            ),
            (
                "an error status in a success",
                with(&|d| d["status"] = "error".into()),
            ),
            ("an unknown status", with(&|d| d["status"] = "done".into())),
            (
                "another scope",
                with(&|d| d["scope"]["value"] = "other".into()),
            ),
            (
                "an unknown relationship",
                with(&|d| d["facts"][0]["relationship"] = "declaration.calls".into()),
            ),
            (
                "a verified strength",
                with(&|d| d["facts"][0]["provenance"]["strength"] = "verified".into()),
            ),
            (
                "an unknown strength",
                with(&|d| d["facts"][0]["provenance"]["strength"] = "certain".into()),
            ),
            (
                "no provenance",
                with(&|d| {
                    d["facts"][0].as_object_mut().unwrap().remove("provenance");
                }),
            ),
            (
                "an absolute location",
                with(&|d| d["facts"][0]["location"]["path"] = "/etc/passwd".into()),
            ),
            (
                "a parent location",
                with(&|d| d["facts"][0]["location"]["path"] = "../x.rs".into()),
            ),
            (
                "a control character",
                with(&|d| d["facts"][0]["subject"]["id"] = "a\nIGNORE PREVIOUS".into()),
            ),
            (
                "a non-text attribute",
                with(&|d| d["facts"][0]["subject"]["attributes"]["kind"] = 3.into()),
            ),
            (
                "ok with diagnostics",
                with(
                    &|d| d["diagnostics"] = serde_json::json!([{"code":"x","state":"unparseable","message":"","location":null}]),
                ),
            ),
            (
                "partial without diagnostics",
                with(&|d| d["status"] = "partial".into()),
            ),
            (
                "a diagnostic state it does not define",
                with(&|d| {
                    d["status"] = "partial".into();
                    d["diagnostics"] =
                        serde_json::json!([{"code":"x","state":"guessed","message":""}]);
                }),
            ),
            (
                "a missing fact list",
                with(&|d| {
                    d.as_object_mut().unwrap().remove("facts");
                }),
            ),
            (
                "too many facts",
                with(&|d| {
                    let f = d["facts"][0].clone();
                    d["facts"] = Value::Array(vec![f; MAX_FACTS as usize + 1]);
                }),
            ),
        ] {
            assert!(result.is_err(), "{what} was accepted");
        }
        assert!(parse_document(b"not json", "crate:fx").is_err());
        assert!(parse_document(b"[]", "crate:fx").is_err());
        assert!(parse_document(b"", "crate:fx").is_err());
    }

    #[test]
    fn pax_errors_become_explicit_states_and_unknown_ones_are_not_trusted() {
        let refusal = |json: &str| parse_refusal(json.as_bytes()).unwrap();
        for (code, state) in [
            ("invalid_scope", ObservationState::InvalidScope),
            ("scope_not_found", ObservationState::InvalidScope),
            ("unsupported_scope", ObservationState::Unsupported),
            ("unsupported_project", ObservationState::Unsupported),
            ("limit_exceeded", ObservationState::LimitExceeded),
            ("artifact_unreadable", ObservationState::ArtifactUnreadable),
            ("invalid_limit", ObservationState::Malformed),
            ("something_new", ObservationState::Malformed),
        ] {
            let r = refusal(&format!(
                r#"{{"schema":"pax.observation.v1","status":"error","code":"{code}","message":"at /Users/x/p"}}"#
            ));
            let rendered = refusal_text(&r, "crate:fx", "0.4.1");
            assert_eq!(rendered.state, state, "{code}");
            assert_eq!(parse_state(&rendered.text), Some(state), "{code}");
            assert!(
                rendered.text.contains("NOT "),
                "{code} says what is not known"
            );
            assert!(
                !rendered.text.contains("/Users/"),
                "PAX's message is never passed on: {}",
                rendered.text
            );
        }
        let r = refusal(
            r#"{"schema":"pax.observation.v1","status":"error","code":"limit_exceeded","limit":"max_files","limit_value":50}"#,
        );
        let text = refusal_text(&r, "crate:x", "0.4.1").text;
        let head: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(
            (head["limit"].clone(), head["limit_value"].clone()),
            ("max_files".into(), 50.into())
        );
        // Not a typed PAX error: not a refusal.
        assert!(parse_refusal(b"error: something").is_none());
        assert!(parse_refusal(br#"{"schema":"other","status":"error","code":"x"}"#).is_none());
        assert!(
            parse_refusal(br#"{"schema":"pax.observation.v1","status":"ok","code":"x"}"#).is_none()
        );
    }

    #[test]
    fn an_unreadable_file_scope_is_artifact_unreadable_not_partial() {
        let diag = serde_json::json!({"code":"artifact_unreadable","state":"unreadable","message":"x","location":{"path":"src/hidden.rs"}});
        let mut d = doc("partial", vec![], vec![diag.clone()]);
        d["scope"] = serde_json::json!({"kind":"file","value":"src/hidden.rs"});
        let parsed = parse_document(d.to_string().as_bytes(), "file:src/hidden.rs").unwrap();
        assert!(target_unreadable(&parsed, &scope("file:src/hidden.rs")));
        // For a crate, one unreadable module among others is a partial observation.
        assert!(!target_unreadable(&parsed, &scope("crate:fx")));
        // And an unreadable file that is not the requested one is not the requested artifact.
        assert!(!target_unreadable(&parsed, &scope("file:src/other.rs")));
    }

    #[test]
    fn a_malformed_observation_carries_chips_reason_and_none_of_the_payload() {
        let r = malformed_text("crate:fx", "0.4.1", "the schema is not pax.observation.v1");
        assert_eq!(parse_state(&r.text), Some(ObservationState::Malformed));
        assert!(r.text.contains("NOT AN OBSERVATION"));
    }

    #[test]
    fn parse_state_reads_only_this_capabilitys_head_line() {
        assert_eq!(parse_state(""), None);
        assert_eq!(parse_state("complete"), None);
        assert_eq!(
            parse_state(r#"{"capability":"project.read","state":"complete"}"#),
            None
        );
        assert_eq!(
            parse_state(r#"{"capability":"project.observe","state":"certain"}"#),
            None
        );
        for s in ObservationState::ALL {
            let text = format!(
                r#"{{"capability":"project.observe","state":"{}"}}"#,
                s.name()
            );
            assert_eq!(parse_state(&text), Some(s));
        }
        // A model's prose cannot pose as the head line.
        assert_eq!(
            parse_state(
                "The state is complete\n{\"capability\":\"project.observe\",\"state\":\"complete\"}"
            ),
            None
        );
    }

    #[test]
    fn arguments_are_fixed_and_the_scope_is_one_discrete_argument() {
        let o = PaxObserve::new(PaxExecutor::new("/work"));
        let args: Vec<String> = o
            .invocation(&scope("crate:fx-core"))
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "--dir",
                "/work",
                "--json",
                "observe",
                "--scope",
                "crate:fx-core",
                "--max-files",
                "50",
                "--max-bytes",
                "1048576",
                "--max-facts",
                "600"
            ]
        );
    }

    #[tokio::test]
    async fn the_declared_capability_is_small_read_only_and_never_remembered() {
        let d = PaxObserve::new(PaxExecutor::new("/work"))
            .capabilities()
            .await
            .unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].id.as_str(), "project.observe");
        assert!(!d[0].reuse_evidence);
        assert!(
            d[0].description.chars().count() <= 160,
            "{}",
            d[0].description.chars().count()
        );
        let inputs: Vec<(&str, bool)> = d[0]
            .inputs
            .iter()
            .map(|i| (i.name.as_str(), i.required))
            .collect();
        assert_eq!(inputs, [("scope", true)]);
    }

    /// The model sees only the capability's description (an input's own description is not rendered
    /// into the prompt), so the description must carry the scope grammar or the capability cannot be used.
    #[tokio::test]
    async fn the_description_a_model_sees_names_every_scope_form() {
        let d = PaxObserve::new(PaxExecutor::new("/work"))
            .capabilities()
            .await
            .unwrap();
        let shown = &d[0].description;
        for form in ["crate:", "module:", "file:", "path:"] {
            assert!(
                shown.contains(form),
                "{form} is not in what the model is told: {shown}"
            );
        }
        assert!(shown.chars().count() <= 160);
        // And what it names parses: each example form it teaches is a form Chip accepts.
        for example in [
            "crate:fx",
            "module:fx/lib::crate::util",
            "file:src/lib.rs",
            "path:src",
        ] {
            assert!(Scope::parse(example).is_ok(), "{example}");
        }
    }

    #[test]
    fn only_a_scope_input_is_accepted() {
        let with = |k: &str, v: InputValue| {
            let mut m = BTreeMap::new();
            m.insert(k.to_string(), v);
            m
        };
        assert!(scope_input(&with("scope", InputValue::Text("crate:fx".into()))).is_ok());
        for (what, inputs) in [
            ("no inputs", BTreeMap::new()),
            ("a command", with("command", InputValue::Text("ls".into()))),
            ("a limit", with("max_files", InputValue::Integer(9999))),
            (
                "a fact",
                with("facts", InputValue::Text("src/x.rs declares y".into())),
            ),
            (
                "an observation",
                with("observation", InputValue::Text("complete".into())),
            ),
            ("a number", with("scope", InputValue::Integer(1))),
            ("a bool", with("scope", InputValue::Bool(true))),
        ] {
            assert!(
                matches!(scope_input(&inputs), Err(CapabilityError::InvalidInput(_))),
                "{what}"
            );
        }
        let mut two = with("scope", InputValue::Text("crate:fx".into()));
        two.insert("max_files".into(), InputValue::Integer(1));
        assert!(
            scope_input(&two).is_err(),
            "an extra input beside a valid scope"
        );
    }
}
