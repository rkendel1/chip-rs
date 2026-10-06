//! Chip → Compute adapter.
//!
//! `ComputeExecutor` implements Chip's `Executor` by invoking the supported
//! Compute entry point, `compute exec <entrypoint> --runtime <rt> --json`,
//! and translating its structured result. Compute stays authoritative for
//! execution: this crate keeps no state about executions.
//!
//! An `ExecutionRequest.intent` is a *name*, never a command line. The adapter
//! maps it to a configured `ComputeOperation` (a runtime plus entrypoint source);
//! an intent with no configured operation is an `InvalidRequest`.
//!
//! Cancellation: dropping the `execute` future kills the local `compute`
//! client process and stops waiting. Compute reports `cancellation: unsupported`
//! for its runtimes, so this does NOT claim the underlying workload was cancelled.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chip_core::{
    CapabilityAvailability, CapabilityDescriptor, CapabilityError, CapabilityId,
    CapabilityProvider, ExecutionError, ExecutionId, ExecutionRequest, ExecutionResult, Executor,
};
use serde_json::Value;
use tokio::process::Command;

/// Environment variable naming the `compute` executable (default: `compute` on PATH).
pub const COMPUTE_BIN_ENV: &str = "COMPUTE_BIN";

/// Intent of the built-in deterministic operation.
pub const SELFTEST_INTENT: &str = "compute.selftest";
/// Exact output produced by the built-in operation.
pub const SELFTEST_OUTPUT: &str = "chip-compute selftest ok";

/// A semantic operation Compute can run: a runtime and the entrypoint source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComputeOperation {
    pub runtime: String,
    pub source: String,
    pub args: Vec<String>,
    /// Human-facing description shown as a capability. Implementation details
    /// (runtime, source) are never part of it.
    pub name: String,
    pub description: String,
}

impl ComputeOperation {
    pub fn python(source: impl Into<String>) -> Self {
        Self {
            runtime: "python".into(),
            source: source.into(),
            args: Vec::new(),
            name: String::new(),
            description: String::new(),
        }
    }

    pub fn described(mut self, name: impl Into<String>, description: impl Into<String>) -> Self {
        self.name = name.into();
        self.description = description.into();
        self
    }

    fn extension(&self) -> &'static str {
        match self.runtime.as_str() {
            "python" => "py",
            "node" => "js",
            "bun" => "js",
            "deno" => "js",
            "ruby" => "rb",
            "php" => "php",
            _ => "txt",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ComputeExecutor {
    binary: PathBuf,
    operations: BTreeMap<String, ComputeOperation>,
    timeout: Duration,
}

impl ComputeExecutor {
    /// Builds an executor using `$COMPUTE_BIN` or `compute` from PATH, with the
    /// built-in deterministic operation registered.
    pub fn new() -> Self {
        let binary = std::env::var_os(COMPUTE_BIN_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("compute"));
        Self::with_binary(binary)
    }

    pub fn with_binary(binary: impl Into<PathBuf>) -> Self {
        let mut executor = Self {
            binary: binary.into(),
            operations: BTreeMap::new(),
            timeout: Duration::from_secs(30),
        };
        executor.operations.insert(
            SELFTEST_INTENT.to_string(),
            ComputeOperation::python(format!("print(\"{SELFTEST_OUTPUT}\")\n"))
                .described("Compute Self Test", "Deterministic Compute execution test"),
        );
        executor
    }

    /// Registers an operation under a capability id. The id must be a valid
    /// `CapabilityId`; arbitrary strings are rejected.
    pub fn with_operation(
        mut self,
        capability: impl Into<String>,
        op: ComputeOperation,
    ) -> Result<Self, CapabilityError> {
        let id = CapabilityId::new(capability)?;
        self.operations.insert(id.as_str().to_string(), op);
        Ok(self)
    }

    /// Whether the `compute` executable can be found, without running it.
    fn binary_present(&self) -> bool {
        let is_file = |p: &Path| p.is_file();
        if self.binary.components().count() > 1 || self.binary.is_absolute() {
            return is_file(&self.binary);
        }
        std::env::var_os("PATH")
            .map(|paths| std::env::split_paths(&paths).any(|dir| is_file(&dir.join(&self.binary))))
            .unwrap_or(false)
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

impl Default for ComputeExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl CapabilityProvider for ComputeExecutor {
    /// Describes the configured operations. Runs nothing and touches no files.
    async fn capabilities(&self) -> Result<Vec<CapabilityDescriptor>, CapabilityError> {
        self.operations
            .iter()
            .map(|(id, op)| {
                let name = if op.name.is_empty() {
                    id.clone()
                } else {
                    op.name.clone()
                };
                Ok(CapabilityDescriptor::new(
                    CapabilityId::new(id.clone())?,
                    name,
                    op.description.clone(),
                ))
            })
            .collect()
    }

    async fn availability(&self, id: &CapabilityId) -> CapabilityAvailability {
        let Some(op) = self.operations.get(id.as_str()) else {
            return CapabilityAvailability::Unavailable("capability is not provided".into());
        };
        if op.runtime.trim().is_empty() || op.source.trim().is_empty() {
            return CapabilityAvailability::Misconfigured(
                "capability has no runnable definition".into(),
            );
        }
        if !self.binary_present() {
            return CapabilityAvailability::Unavailable("Compute is not installed".into());
        }
        CapabilityAvailability::Available
    }
}

/// Request translation: the argument vector for `compute`.
pub fn build_arguments(op: &ComputeOperation, entrypoint: &Path, timeout: Duration) -> Vec<String> {
    let mut args = vec![
        "exec".to_string(),
        entrypoint.display().to_string(),
        "--runtime".into(),
        op.runtime.clone(),
        "--timeout".into(),
        format!("{}s", timeout.as_secs().max(1)),
        "--json".into(),
    ];
    args.extend(op.args.iter().cloned());
    args
}

/// Result translation: Compute's `--json` execution result → Chip's result.
pub fn translate_result(id: ExecutionId, stdout: &[u8]) -> Result<ExecutionResult, ExecutionError> {
    let value: Value = serde_json::from_slice(stdout).map_err(|_| {
        ExecutionError::ExecutionFailed("compute returned an unreadable result".into())
    })?;
    let status = value["status"].as_str().unwrap_or("");
    let exit_code = value["exit_code"].as_i64();
    let text = value["stdout"]["text"].as_str().unwrap_or("");
    let receipt = value["receipt"]["receipt_hash"].as_str().map(str::to_owned);

    if status == "cancelled" {
        return Err(ExecutionError::Cancelled);
    }

    let mut result = if status == "completed" && exit_code == Some(0) {
        ExecutionResult::success(id, text.trim_end())
    } else {
        let detail = value["error"]["message"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("exit code {exit_code:?}"));
        let stderr = value["stderr"]["text"].as_str().unwrap_or("").trim_end();
        let output = format!("compute status {status}: {detail}\n{stderr}");
        ExecutionResult::failure(id, output.trim_end())
    };
    if let Some(receipt) = receipt {
        result = result.with_receipt_id(receipt);
    }
    Ok(result)
}

/// Error translation: Compute's failure envelope (last stderr line) → ExecutionError.
pub fn translate_failure(stderr: &str) -> ExecutionError {
    let envelope = stderr
        .lines()
        .rev()
        .find_map(|line| serde_json::from_str::<Value>(line.trim()).ok());
    let code = envelope
        .as_ref()
        .and_then(|v| v["error"]["code"].as_str())
        .unwrap_or("unknown");
    let message = envelope
        .as_ref()
        .and_then(|v| v["error"]["message"].as_str())
        .unwrap_or("compute exited with an error");
    match code {
        "controller_unavailable"
        | "runtime_unavailable"
        | "runtime_version_mismatch"
        | "unknown_runtime"
        | "isolation_unavailable"
        | "unsupported_capability"
        | "state_unavailable"
        | "no_compatible_provider" => {
            ExecutionError::ExecutorUnavailable(format!("{code}: {message}"))
        }
        "invalid_arguments"
        | "invalid_workload"
        | "invalid_bundle"
        | "invalid_mount_path"
        | "invalid_json"
        | "unsupported_workload_version" => {
            ExecutionError::InvalidRequest(format!("{code}: {message}"))
        }
        "cancelled" => ExecutionError::Cancelled,
        _ => ExecutionError::ExecutionFailed(format!("{code}: {message}")),
    }
}

static SCRATCH: AtomicU64 = AtomicU64::new(0);

/// Per-execution directory holding the entrypoint; removed on drop (including cancel).
struct Scratch(PathBuf);

impl Scratch {
    fn create() -> std::io::Result<Self> {
        let dir = std::env::temp_dir().join(format!(
            "chip-compute-{}-{}",
            std::process::id(),
            SCRATCH.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir)?;
        Ok(Self(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[async_trait::async_trait]
impl Executor for ComputeExecutor {
    async fn execute(&self, request: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        let op = self.operations.get(&request.intent).ok_or_else(|| {
            ExecutionError::InvalidRequest(format!(
                "no Compute operation is configured for intent '{}'",
                request.intent
            ))
        })?;

        let scratch = Scratch::create()
            .map_err(|_| ExecutionError::ExecutionFailed("cannot prepare entrypoint".into()))?;
        let entrypoint = scratch.0.join(format!("operation.{}", op.extension()));
        std::fs::write(&entrypoint, &op.source)
            .map_err(|_| ExecutionError::ExecutionFailed("cannot write entrypoint".into()))?;

        let output = Command::new(&self.binary)
            .args(build_arguments(op, &entrypoint, self.timeout))
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::NotFound => ExecutionError::ExecutorUnavailable(format!(
                    "compute executable '{}' not found",
                    self.binary.display()
                )),
                _ => ExecutionError::ExecutorUnavailable("cannot start compute".into()),
            })?;

        if output.stdout.is_empty() {
            return Err(translate_failure(&String::from_utf8_lossy(&output.stderr)));
        }
        translate_result(request.id, &output.stdout)
    }
}
