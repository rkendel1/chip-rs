//! Chip → PAX adapter.
//!
//! `PaxExecutor` makes one external capability, `pax.test`, available to Chip and executes it by
//! running an independently installed PAX binary. PAX is not embedded, reimplemented or modified.
//! PAX decides what the project's native tooling did; this adapter only consumes that decision.
//!
//! # Authority
//!
//! * **The model** may *name* `pax.test`. It supplies nothing else: the capability declares no
//!   inputs, and Chip's invocation validation rejects every field a request might add.
//! * **Chip** resolves the executable (verifying it is PAX, and PAX 0.3.0 or later), fixes the
//!   argument vector (`pax --dir <work directory> --json test`), takes the work directory from
//!   runtime configuration, and decides what the observation means for the goal.
//! * **PAX** runs the native tooling and states the outcome in `pax.execution-result.v1`.
//!
//! # The result contract
//!
//! With `--json`, PAX writes exactly one `pax.execution-result.v1` document to stdout; the native
//! tool's output and PAX's diagnostics go to stderr. This adapter:
//!
//! * parses **stdout only**, strictly ([`parse_execution_result`]); stderr is kept as diagnostics
//!   and is never read for meaning;
//! * keeps PAX's semantic `status` and the native `exit_code` as separate facts: `not_run` with
//!   exit code 0 is a different observation from `passed` with exit code 0;
//! * does not turn an exit code, Cargo's text, or anything a model wrote into a result;
//! * produces **no observation at all** when stdout is not a valid result: an execution whose
//!   outcome cannot be established is a failure, not evidence.
//!
//! The observation is Chip's own canonical rendering of the validated result, so nothing in
//! stderr or in a project's output can pose as a status. It carries no receipt: PAX issues none,
//! and a PAX result is an observation, not a cryptographic receipt.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use chip_core::{
    CapabilityAvailability, CapabilityDescriptor, CapabilityError, CapabilityId,
    CapabilityProvider, ExecutionError, ExecutionRequest, ExecutionResult, ExecutionStatus,
    Executor, Observation, ObservationKind, ObservationPredicate,
};
use serde::de::{Deserialize, Deserializer, MapAccess, Visitor};
use tokio::process::Command;

/// The one capability this adapter provides.
pub const PAX_TEST_CAPABILITY: &str = "pax.test";

/// Environment variable naming the PAX executable explicitly (like `COMPUTE_BIN` for Compute).
/// It is Chip's own configuration; no model output can set it.
pub const PAX_BIN_ENV: &str = "PAX_BIN";

/// The only result schema this adapter understands. Any other value is rejected, never downgraded.
pub const RESULT_SCHEMA: &str = "pax.execution-result.v1";

/// The oldest PAX that provides [`RESULT_SCHEMA`].
pub const MIN_PAX_VERSION: (u64, u64, u64) = (0, 3, 0);

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Output kept per stream. A result larger than this cannot be trusted whole and is refused.
const MAX_STREAM_BYTES: usize = 256 * 1024;

// ---- versions ---------------------------------------------------------------------------------

/// A semantic version, enough to compare PAX's against the minimum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// The pre-release part, if any (`rc.1` in `0.3.0-rc.1`). Build metadata is dropped.
    pub pre: Option<String>,
}

impl Version {
    /// `MAJOR.MINOR.PATCH[-PRERELEASE][+BUILD]`; the numbers have no leading zeros.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.split_once('+').map_or(text, |(core, build)| {
            // Build metadata must be well formed even though it is ignored.
            if build.is_empty()
                || !build
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
            {
                "!"
            } else {
                core
            }
        });
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (text, None),
        };
        if let Some(pre) = pre
            && (pre.is_empty()
                || !pre
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.'))
        {
            return None;
        }
        let number = |part: &str| -> Option<u64> {
            let valid = !part.is_empty()
                && part.chars().all(|c| c.is_ascii_digit())
                && (part == "0" || !part.starts_with('0'));
            valid.then(|| part.parse().ok()).flatten()
        };
        let mut parts = core.split('.');
        let (major, minor, patch) = (
            number(parts.next()?)?,
            number(parts.next()?)?,
            number(parts.next()?)?,
        );
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            major,
            minor,
            patch,
            pre: pre.map(str::to_string),
        })
    }

    /// Whether this version is `minimum` or later. A pre-release of the minimum is earlier than it.
    pub fn at_least(&self, minimum: (u64, u64, u64)) -> bool {
        match (self.major, self.minor, self.patch).cmp(&minimum) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Equal => self.pre.is_none(),
            std::cmp::Ordering::Less => false,
        }
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if let Some(pre) = &self.pre {
            write!(f, "-{pre}")?;
        }
        Ok(())
    }
}

// ---- the result contract ------------------------------------------------------------------------

/// What PAX established about the operation. All six are preserved; none is a boolean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaxStatus {
    Passed,
    Failed,
    Error,
    Unsupported,
    Ambiguous,
    NotRun,
}

impl PaxStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Error => "error",
            Self::Unsupported => "unsupported",
            Self::Ambiguous => "ambiguous",
            Self::NotRun => "not_run",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "passed" => Self::Passed,
            "failed" => Self::Failed,
            "error" => Self::Error,
            "unsupported" => Self::Unsupported,
            "ambiguous" => Self::Ambiguous,
            "not_run" => Self::NotRun,
            _ => return None,
        })
    }
}

/// Test counts, when PAX reports them (it omits them rather than guess).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TestCounts {
    pub passed: u64,
    pub failed: u64,
    pub ignored: u64,
    pub measured: u64,
}

/// A validated `pax.execution-result.v1` for the `test` operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaxExecutionResult {
    /// What PAX established. This, and only this, is the semantic result.
    pub status: PaxStatus,
    /// PAX's stable machine code refining `status`. Diagnostic: it never changes the status.
    pub reason: String,
    /// The native tool PAX selected, or `None` if none was.
    pub tool: Option<String>,
    /// The native process's exit status, unmodified; `None` if no native process ran to an exit.
    /// Not a semantic result.
    pub exit_code: Option<i64>,
    pub tests: Option<TestCounts>,
}

/// Why stdout is not a valid result. Every variant means "no observation".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaxResultError {
    Empty,
    NotUtf8,
    /// Not exactly one JSON document, or not JSON.
    Malformed(String),
    NotAnObject,
    DuplicateField(String),
    MissingField(&'static str),
    WrongType(&'static str),
    WrongSchema(String),
    WrongOperation(String),
    InvalidStatus(String),
    InvalidReason,
    InvalidTool,
    InvalidExitCode,
    InvalidTests(String),
}

impl std::fmt::Display for PaxResultError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "stdout is empty"),
            Self::NotUtf8 => write!(f, "stdout is not UTF-8"),
            Self::Malformed(e) => write!(f, "stdout is not exactly one JSON document: {e}"),
            Self::NotAnObject => write!(f, "the result is not a JSON object"),
            Self::DuplicateField(k) => write!(f, "field `{k}` appears more than once"),
            Self::MissingField(k) => write!(f, "field `{k}` is missing"),
            Self::WrongType(k) => write!(f, "field `{k}` has the wrong type"),
            Self::WrongSchema(s) => write!(f, "schema `{s}` is not `{RESULT_SCHEMA}`"),
            Self::WrongOperation(o) => write!(f, "operation `{o}` is not `test`"),
            Self::InvalidStatus(s) => write!(f, "status `{s}` is not a PAX status"),
            Self::InvalidReason => write!(f, "reason is not a machine code"),
            Self::InvalidTool => write!(f, "tool is not a tool name"),
            Self::InvalidExitCode => write!(f, "exit_code is neither null nor an integer"),
            Self::InvalidTests(e) => write!(f, "tests is invalid: {e}"),
        }
    }
}

/// The top-level object's entries in order, duplicates kept so they can be refused (a plain map
/// would silently keep the last of two `status` fields).
struct Entries(Vec<(String, serde_json::Value)>);

impl<'de> Deserialize<'de> for Entries {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Entries;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "a JSON object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Entries, A::Error> {
                let mut entries = Vec::new();
                while let Some(entry) = map.next_entry::<String, serde_json::Value>()? {
                    entries.push(entry);
                }
                Ok(Entries(entries))
            }
        }
        deserializer.deserialize_map(V)
    }
}

fn machine_code(text: &str, max: usize, extra: &[char]) -> bool {
    !text.is_empty()
        && text.len() <= max
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || extra.contains(&c))
}

/// Parses PAX's stdout as one `pax.execution-result.v1` for `test`, strictly.
///
/// Required: `schema == "pax.execution-result.v1"`, `operation == "test"`, a `status` that is one
/// of PAX's six, a `reason`, a `tool` (string or null) and an `exit_code` (integer or null). `tests`
/// is optional but, when present, must hold the four counts. Refused: anything that is not exactly
/// one document, a repeated field, a missing or mistyped field, any other schema or operation.
/// Fields the contract does not define are ignored, never interpreted.
pub fn parse_execution_result(stdout: &[u8]) -> Result<PaxExecutionResult, PaxResultError> {
    if stdout.iter().all(u8::is_ascii_whitespace) {
        return Err(PaxResultError::Empty);
    }
    let text = std::str::from_utf8(stdout).map_err(|_| PaxResultError::NotUtf8)?;
    let entries = serde_json::from_str::<Entries>(text)
        .map_err(|e| {
            if e.is_data() && e.to_string().contains("JSON object") {
                PaxResultError::NotAnObject
            } else {
                PaxResultError::Malformed(e.to_string())
            }
        })?
        .0;
    for (i, (key, _)) in entries.iter().enumerate() {
        if entries[..i].iter().any(|(k, _)| k == key) {
            return Err(PaxResultError::DuplicateField(key.clone()));
        }
    }
    let get = |name: &'static str| -> Result<&serde_json::Value, PaxResultError> {
        entries
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v)
            .ok_or(PaxResultError::MissingField(name))
    };
    let string = |name: &'static str| -> Result<&str, PaxResultError> {
        get(name)?.as_str().ok_or(PaxResultError::WrongType(name))
    };

    let schema = string("schema")?;
    if schema != RESULT_SCHEMA {
        return Err(PaxResultError::WrongSchema(
            schema.chars().take(64).collect(),
        ));
    }
    let operation = string("operation")?;
    if operation != "test" {
        return Err(PaxResultError::WrongOperation(
            operation.chars().take(64).collect(),
        ));
    }
    let status_text = string("status")?;
    let status = PaxStatus::parse(status_text)
        .ok_or_else(|| PaxResultError::InvalidStatus(status_text.chars().take(64).collect()))?;
    let reason = string("reason")?;
    if !machine_code(reason, 64, &[]) {
        return Err(PaxResultError::InvalidReason);
    }
    let tool = match get("tool")? {
        serde_json::Value::Null => None,
        serde_json::Value::String(t) if machine_code(t, 64, &['.', '_']) => Some(t.clone()),
        serde_json::Value::String(_) => return Err(PaxResultError::InvalidTool),
        _ => return Err(PaxResultError::WrongType("tool")),
    };
    let exit_code = match get("exit_code")? {
        serde_json::Value::Null => None,
        serde_json::Value::Number(n) => Some(n.as_i64().ok_or(PaxResultError::InvalidExitCode)?),
        _ => return Err(PaxResultError::InvalidExitCode),
    };
    let tests = match entries.iter().find(|(k, _)| k == "tests").map(|(_, v)| v) {
        None => None,
        Some(serde_json::Value::Object(counts)) => {
            let count = |name: &str| -> Result<u64, PaxResultError> {
                counts
                    .get(name)
                    .and_then(serde_json::Value::as_u64)
                    .ok_or_else(|| {
                        PaxResultError::InvalidTests(format!(
                            "`{name}` is not a non-negative integer"
                        ))
                    })
            };
            Some(TestCounts {
                passed: count("passed")?,
                failed: count("failed")?,
                ignored: count("ignored")?,
                measured: count("measured")?,
            })
        }
        Some(_) => return Err(PaxResultError::InvalidTests("not an object".into())),
    };
    Ok(PaxExecutionResult {
        status,
        reason: reason.to_string(),
        tool,
        exit_code,
        tests,
    })
}

impl PaxExecutionResult {
    /// Chip's canonical rendering: one line, fixed key order, nothing PAX wrote beyond the
    /// validated fields. The strings in it are machine codes, so no escaping can be needed.
    pub fn canonical_json(&self) -> String {
        let tool = self
            .tool
            .as_ref()
            .map_or("null".to_string(), |t| format!("\"{t}\""));
        let exit = self.exit_code.map_or("null".to_string(), |c| c.to_string());
        let tests = self.tests.map_or(String::new(), |t| {
            format!(
                ",\"tests\":{{\"passed\":{},\"failed\":{},\"ignored\":{},\"measured\":{}}}",
                t.passed, t.failed, t.ignored, t.measured
            )
        });
        format!(
            "{{\"schema\":\"{RESULT_SCHEMA}\",\"operation\":\"test\",\"status\":\"{}\",\"reason\":\"{}\",\"tool\":{tool},\"exit_code\":{exit}{tests}}}",
            self.status.as_str(),
            self.reason
        )
    }
}

/// The observation text for one PAX run: line one is the canonical validated result; then PAX's own
/// process status; then stderr, labelled diagnostics. Only line one is ever read for meaning.
pub fn render_observation(
    result: &PaxExecutionResult,
    pax_process_exit: Option<i32>,
    stderr: &[u8],
) -> String {
    let kept = &stderr[..stderr.len().min(MAX_STREAM_BYTES)];
    format!(
        "{}\npax_process_exit: {}\n--- stderr (diagnostics only; never evaluated) ---\n{}",
        result.canonical_json(),
        pax_process_exit.map_or("none (terminated by a signal)".to_string(), |c| c
            .to_string()),
        String::from_utf8_lossy(kept),
    )
}

/// The goal requirement "the PAX test operation is established as passed".
///
/// It reads the observation's first line, which Chip rendered from a validated result, and asks
/// whether PAX's `status` is `passed`. It does not look at `exit_code` (exit 0 with `not_run` is
/// not a pass), at `reason`, at stderr, or at anything else.
#[derive(Debug, Clone, Copy, Default)]
pub struct PaxTestPassed;

impl ObservationPredicate for PaxTestPassed {
    fn describe(&self) -> String {
        "the PAX test operation is established as passed (pax.execution-result.v1 status = passed)"
            .to_string()
    }

    fn satisfied_by(&self, observation: &Observation) -> bool {
        observation.kind == ObservationKind::ExecutionCompleted
            && observation.status == ExecutionStatus::Success
            && observation
                .output
                .as_deref()
                .and_then(|text| text.lines().next())
                .and_then(|line| parse_execution_result(line.as_bytes()).ok())
                .is_some_and(|result| result.status == PaxStatus::Passed)
    }
}

// ---- resolving PAX -----------------------------------------------------------------------------------

/// Why PAX cannot be used. Every variant means "do not run anything".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaxUnavailable {
    /// No `pax` was found by the configured path or on the search path.
    NotFound(String),
    /// A candidate was found but did not identify itself as PAX.
    NotPax { path: PathBuf, reason: String },
    /// It is PAX, but older than the version that provides the result contract.
    TooOld {
        path: PathBuf,
        found: String,
        required: String,
    },
    /// The work directory is not a directory.
    BadWorkDirectory(PathBuf),
}

impl std::fmt::Display for PaxUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(why) => write!(f, "PAX is not installed: {why}"),
            Self::NotPax { path, reason } => write!(f, "{} is not PAX: {reason}", path.display()),
            Self::TooOld {
                path,
                found,
                required,
            } => write!(
                f,
                "{} is PAX {found}, but {RESULT_SCHEMA} needs PAX {required} or later",
                path.display()
            ),
            Self::BadWorkDirectory(dir) => {
                write!(f, "the work directory {} is not a directory", dir.display())
            }
        }
    }
}

/// A PAX executable that identified itself, at a version that provides the contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPax {
    pub path: PathBuf,
    /// What the identity probe printed after `pax `, e.g. `0.3.0`.
    pub version: String,
}

#[derive(Debug, Clone)]
pub struct PaxExecutor {
    explicit_binary: Option<PathBuf>,
    search_path: Option<OsString>,
    work_directory: PathBuf,
    timeout: Duration,
}

impl PaxExecutor {
    /// Runs PAX against `work_directory`. The binary is `$PAX_BIN` if set, otherwise the first
    /// `pax` on `$PATH`.
    pub fn new(work_directory: impl Into<PathBuf>) -> Self {
        Self {
            explicit_binary: std::env::var_os(PAX_BIN_ENV)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
            search_path: std::env::var_os("PATH"),
            work_directory: work_directory.into(),
            timeout: Duration::from_secs(300),
        }
    }

    /// Names the executable explicitly. It is still identity- and version-checked.
    pub fn with_binary(mut self, path: impl Into<PathBuf>) -> Self {
        self.explicit_binary = Some(path.into());
        self
    }

    /// Replaces the search path (a `PATH`-style list). For deterministic lookup in tests and
    /// embedding; the process environment is never modified.
    pub fn with_search_path(mut self, path: impl Into<OsString>) -> Self {
        self.explicit_binary = None;
        self.search_path = Some(path.into());
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn work_directory(&self) -> &Path {
        &self.work_directory
    }

    /// The arguments PAX is run with. Fixed: nothing a model wrote is in them.
    pub fn invocation(&self) -> Vec<OsString> {
        vec![
            OsString::from("--dir"),
            self.work_directory.clone().into_os_string(),
            OsString::from("--json"),
            OsString::from("test"),
        ]
    }

    /// The candidate executable: the explicit one, or the *first* `pax` file on the search path.
    /// Later candidates are never considered: a first one that is not PAX is a failure, not a
    /// reason to look further.
    fn candidate(&self) -> Result<PathBuf, PaxUnavailable> {
        if let Some(path) = &self.explicit_binary {
            return if path.is_file() {
                Ok(path.clone())
            } else {
                Err(PaxUnavailable::NotFound(format!(
                    "{} is not a file",
                    path.display()
                )))
            };
        }
        let search = self
            .search_path
            .as_deref()
            .ok_or_else(|| PaxUnavailable::NotFound("there is no search path".into()))?;
        std::env::split_paths(search)
            .map(|dir| dir.join("pax"))
            .find(|candidate| candidate.is_file())
            .ok_or_else(|| PaxUnavailable::NotFound("no `pax` on the search path".into()))
    }

    /// Finds PAX, verifies that it is PAX, and that it is new enough. Fails closed.
    pub async fn resolve(&self) -> Result<ResolvedPax, PaxUnavailable> {
        if !self.work_directory.is_dir() {
            return Err(PaxUnavailable::BadWorkDirectory(
                self.work_directory.clone(),
            ));
        }
        let path = self.candidate()?;
        let version = probe_identity(&path).await?;
        let parsed = Version::parse(&version).ok_or_else(|| PaxUnavailable::NotPax {
            path: path.clone(),
            reason: "its `--version` is not `pax <version>`".into(),
        })?;
        if !parsed.at_least(MIN_PAX_VERSION) {
            let (a, b, c) = MIN_PAX_VERSION;
            return Err(PaxUnavailable::TooOld {
                path,
                found: version,
                required: format!("{a}.{b}.{c}"),
            });
        }
        Ok(ResolvedPax { path, version })
    }
}

/// Runs `<path> --version` and requires exactly one line, `pax <semver>`. The POSIX archive
/// utility rejects `--version`; anything else that does not say it is PAX is refused.
async fn probe_identity(path: &Path) -> Result<String, PaxUnavailable> {
    let not_pax = |reason: &str| PaxUnavailable::NotPax {
        path: path.to_path_buf(),
        reason: reason.to_string(),
    };
    let probe = Command::new(path)
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = match tokio::time::timeout(PROBE_TIMEOUT, probe).await {
        Err(_) => return Err(not_pax("the identity probe did not finish")),
        Ok(Err(_)) => return Err(not_pax("the identity probe could not be started")),
        Ok(Ok(output)) => output,
    };
    if !output.status.success() {
        return Err(not_pax("it did not accept `--version`"));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.trim();
    let version = line
        .strip_prefix("pax ")
        .filter(|v| !line.contains('\n') && Version::parse(v).is_some())
        .ok_or_else(|| not_pax("its `--version` is not `pax <version>`"))?;
    Ok(version.to_string())
}

// ---- the capability -------------------------------------------------------------------------------------

#[async_trait::async_trait]
impl CapabilityProvider for PaxExecutor {
    /// Describes the capability. Runs nothing. The description says what it does for the work, not
    /// how any ecosystem does it: PAX owns that.
    async fn capabilities(&self) -> Result<Vec<CapabilityDescriptor>, CapabilityError> {
        // The project's state changes between requests, so a result is never reused for a later
        // request: the tests are run again, and what they say now is what is observed.
        Ok(vec![
            CapabilityDescriptor::new(
                CapabilityId::new(PAX_TEST_CAPABILITY)?,
                "PAX test",
                "Run the current project's tests.",
            )
            .without_evidence_reuse(),
        ])
    }

    async fn availability(&self, id: &CapabilityId) -> CapabilityAvailability {
        if id.as_str() != PAX_TEST_CAPABILITY {
            return CapabilityAvailability::Unavailable("capability is not provided".into());
        }
        match self.resolve().await {
            Ok(_) => CapabilityAvailability::Available,
            Err(why) => CapabilityAvailability::Unavailable(why.to_string()),
        }
    }
}

#[async_trait::async_trait]
impl Executor for PaxExecutor {
    async fn execute(&self, request: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        if request.intent != PAX_TEST_CAPABILITY {
            return Err(ExecutionError::InvalidRequest(format!(
                "'{}' is not a capability this executor provides",
                request.intent
            )));
        }
        // `pax.test` declares no inputs, so none may arrive: nothing a requester supplied can
        // reach the PAX invocation.
        if !request.inputs.is_empty() {
            return Err(ExecutionError::InvalidRequest(
                "pax.test takes no inputs".into(),
            ));
        }
        // Verified at the moment of use, and the path verified is the path run.
        let pax = self
            .resolve()
            .await
            .map_err(|why| ExecutionError::ExecutorUnavailable(why.to_string()))?;
        let child = Command::new(&pax.path)
            .args(self.invocation())
            .current_dir(&self.work_directory)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output();
        let output = match tokio::time::timeout(self.timeout, child).await {
            Err(_) => {
                return Err(ExecutionError::ExecutionFailed(
                    "pax did not finish within the time limit".into(),
                ));
            }
            Ok(Err(_)) => {
                return Err(ExecutionError::ExecutorUnavailable(
                    "pax could not be started".into(),
                ));
            }
            Ok(Ok(output)) => output,
        };
        // The result is on stdout, and only there. A result too large to hold whole is refused:
        // a truncated document cannot be trusted.
        if output.stdout.len() > MAX_STREAM_BYTES {
            return Err(ExecutionError::ExecutionFailed(
                "pax's stdout exceeds the size a result may have".into(),
            ));
        }
        // No valid result, no observation: the outcome was not established, so nothing is recorded.
        let result = parse_execution_result(&output.stdout).map_err(|why| {
            ExecutionError::ExecutionFailed(format!(
                "pax did not produce a valid {RESULT_SCHEMA}: {why}"
            ))
        })?;
        let text = render_observation(&result, output.status.code(), &output.stderr);
        // Only `passed` is a successful observation. Every other status is preserved in the text,
        // and none of them is a success. No receipt: PAX issues none.
        Ok(if result.status == PaxStatus::Passed {
            ExecutionResult::success(request.id, text)
        } else {
            ExecutionResult::failure(request.id, text)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap_or_else(|| panic!("{text} should parse"))
    }

    #[test]
    fn versions_compare_as_numbers_not_strings() {
        // `0.10.0` is later than `0.3.0` although "0.10.0" < "0.3.0" as text.
        for ok in [
            "0.3.0",
            "0.3.1",
            "0.4.0",
            "0.10.0",
            "0.100.7",
            "1.0.0",
            "2.0.0-rc.1",
            "0.3.0+build.5",
            "0.3.1-rc.1",
        ] {
            assert!(
                v(ok).at_least(MIN_PAX_VERSION),
                "{ok} should satisfy >= 0.3.0"
            );
        }
        for old in [
            "0.2.0",
            "0.2.99",
            "0.2.10",
            "0.0.1",
            "0.3.0-rc.1",
            "0.3.0-alpha",
            "0.1.0+3.0.0",
        ] {
            assert!(
                !v(old).at_least(MIN_PAX_VERSION),
                "{old} should not satisfy >= 0.3.0"
            );
        }
    }

    #[test]
    fn malformed_versions_do_not_parse() {
        for bad in [
            "", "0.3", "0.3.0.1", "a.b.c", "0.3.x", "03.0.0", "0.03.0", "0.3.0-", "0.3.0+",
            "-0.3.0", "0.3.0 ", "v0.3.0", "0.3.-1", "0.3.0-é",
        ] {
            assert!(Version::parse(bad).is_none(), "{bad:?} parsed");
        }
        assert_eq!(v("0.3.0+build").pre, None);
        assert_eq!(v("0.3.0-rc.1+b").pre.as_deref(), Some("rc.1"));
        assert_eq!(v("0.3.0-rc.1").to_string(), "0.3.0-rc.1");
    }
}
