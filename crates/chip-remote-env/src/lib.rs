//! Rust Chip's capabilities, executed in an external execution environment.
//!
//! This crate knows one thing about the outside world: it can run a command there, given an argv
//! and an environment, and get back its exit status and captured output ([`CommandRunner`]). There
//! is no stdin. It does not know what provides that (a container, a hosted machine, a remote
//! machine), and it knows nothing about any particular product.
//!
//! Rust Chip keeps every decision about what a capability means. [`RemoteCapabilityBackend`] is a
//! `CapabilityBackend` like any other: Chip validates the request, then the backend asks the
//! environment to run **Rust Chip's own executor** for that capability (the [`worker`], shipped as
//! `chip capability-exec`) against the project in that environment. The environment performs
//! the process; the meaning of `project.write`, the shape of its observation, the validation of its
//! inputs and the interpretation of `pax.test` stay exactly what they are locally, by construction.
//!
//! A [`RemoteEnvironment`] is the environment-contract side: one work's capabilities, observation
//! invariants and opaque identity, all operating inside one external environment.
//!
//! The command's own receipt (whatever the environment records about having run it) is the
//! environment's evidence that it executed something. It is not a Chip receipt and never settles a
//! goal: the observation Chip interprets is the worker's output.

mod protocol;
pub mod worker;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use chip_core::{
    CapabilityAvailability, CapabilityDescriptor, CapabilityError, CapabilityId,
    CapabilityProvider, CapabilitySet, EnvironmentDescription, EnvironmentError, EnvironmentId,
    ExecutionError, ExecutionRequest, ExecutionResult, Executor, InputValue, ObservationInvariant,
    WorkEnvironment,
};
use chip_pax::PaxExecutor;
use chip_project::{
    ProjectExecutor, git_observation_invariant, git_scope_invariant, host_path_leak_invariant,
    navigation_mismatch_invariant, out_of_root_write_invariant, path_escape_invariant,
};

pub use protocol::{Request, Response, WORKER_ENV, WORKER_SUBCOMMAND};

/// What running a command in the environment returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// `None` when the command did not exit normally (killed, timed out).
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// The environment could not run the command at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerError(pub String);

impl std::fmt::Display for RunnerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// `exec(argv, env)` with captured output, in one particular environment. No stdin, no shell.
#[async_trait::async_trait]
pub trait CommandRunner: Send + Sync {
    async fn run(
        &self,
        argv: Vec<String>,
        env: BTreeMap<String, String>,
    ) -> Result<CommandOutput, RunnerError>;
}

/// Where the worker is, and which project (relative to the command's working directory) it
/// operates on. Neither is ever shown to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerCommand {
    pub program: String,
    pub root: String,
}

impl WorkerCommand {
    fn argv(&self) -> Vec<String> {
        vec![
            self.program.clone(),
            WORKER_SUBCOMMAND.to_string(),
            "--root".to_string(),
            self.root.clone(),
        ]
    }
}

/// A request is carried in one environment variable; Linux caps one at 128 KiB. A request that does
/// not fit is refused, never truncated.
pub const MAX_REQUEST_BYTES: usize = 120 * 1024;

async fn call(
    runner: &dyn CommandRunner,
    worker: &WorkerCommand,
    request: &Request,
) -> Result<Response, String> {
    let encoded = request.encode();
    if encoded.len() > MAX_REQUEST_BYTES {
        return Err(format!(
            "the request is {} bytes; the environment carries at most {MAX_REQUEST_BYTES}",
            encoded.len()
        ));
    }
    let mut env = BTreeMap::new();
    env.insert(WORKER_ENV.to_string(), encoded);
    let output = runner
        .run(worker.argv(), env)
        .await
        .map_err(|e| format!("the environment could not run the command: {e}"))?;
    if output.exit_code != Some(0) {
        return Err(format!(
            "the capability worker did not complete (exit {:?})",
            output.exit_code
        ));
    }
    Response::decode(&output.stdout)
}

/// Capabilities whose execution happens in an external environment.
pub struct RemoteCapabilityBackend {
    runner: Arc<dyn CommandRunner>,
    worker: WorkerCommand,
    descriptors: Vec<CapabilityDescriptor>,
}

impl RemoteCapabilityBackend {
    /// Asks the environment which capabilities Rust Chip's executors declare there, once.
    pub async fn connect(
        runner: Arc<dyn CommandRunner>,
        worker: WorkerCommand,
    ) -> Result<Self, EnvironmentError> {
        let response = call(runner.as_ref(), &worker, &Request::Capabilities)
            .await
            .map_err(EnvironmentError::Unavailable)?;
        match response {
            Response::Capabilities(descriptors) => Ok(Self {
                runner,
                worker,
                descriptors,
            }),
            other => Err(EnvironmentError::Unavailable(format!(
                "unexpected answer to a capabilities request: {other:?}"
            ))),
        }
    }
}

#[async_trait::async_trait]
impl CapabilityProvider for RemoteCapabilityBackend {
    async fn capabilities(&self) -> Result<Vec<CapabilityDescriptor>, CapabilityError> {
        Ok(self.descriptors.clone())
    }

    async fn availability(&self, id: &CapabilityId) -> CapabilityAvailability {
        let request = Request::Availability {
            capability: id.to_string(),
        };
        match call(self.runner.as_ref(), &self.worker, &request).await {
            Ok(Response::Availability(availability)) => availability,
            Ok(other) => CapabilityAvailability::Unavailable(format!(
                "unexpected answer to an availability request: {other:?}"
            )),
            Err(why) => CapabilityAvailability::Unavailable(why),
        }
    }

    async fn validate_inputs(
        &self,
        id: &CapabilityId,
        inputs: &BTreeMap<String, InputValue>,
    ) -> Result<(), CapabilityError> {
        let request = Request::Validate {
            capability: id.to_string(),
            inputs: inputs.clone(),
        };
        match call(self.runner.as_ref(), &self.worker, &request).await {
            Ok(Response::Validated(result)) => result,
            Ok(other) => Err(CapabilityError::Unavailable(format!(
                "unexpected answer to a validation request: {other:?}"
            ))),
            Err(why) => Err(CapabilityError::Unavailable(why)),
        }
    }
}

#[async_trait::async_trait]
impl Executor for RemoteCapabilityBackend {
    async fn execute(&self, request: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        let id = request.id.clone();
        let wire = Request::Execute {
            execution_id: request.id.to_string(),
            capability: request.intent,
            inputs: request.inputs,
        };
        match call(self.runner.as_ref(), &self.worker, &wire).await {
            Ok(Response::Executed(result)) => {
                let result = result?;
                // The worker answers for the execution it was given; anything else is refused.
                if result.id != id {
                    return Err(ExecutionError::ExecutionFailed(
                        "the environment answered for a different execution".into(),
                    ));
                }
                Ok(result)
            }
            Ok(other) => Err(ExecutionError::ExecutorUnavailable(format!(
                "unexpected answer to an execution request: {other:?}"
            ))),
            // The environment could not run it: nothing is known to have happened, and nothing is
            // retried or run anywhere else.
            Err(why) => Err(ExecutionError::ExecutorUnavailable(why)),
        }
    }
}

/// The project capabilities and `pax.test`, over one project directory. The one place the local and
/// the remote side agree on what an environment for coding work consists of.
pub fn project_capability_set(root: &Path, pax: PaxExecutor) -> CapabilitySet {
    CapabilitySet::new()
        .with(Arc::new(ProjectExecutor::new(root)))
        .with(Arc::new(pax))
}

/// The observation invariants for a project at `root`.
pub fn project_invariants(root: &Path) -> Vec<Arc<dyn ObservationInvariant>> {
    vec![
        path_escape_invariant(root),
        out_of_root_write_invariant(root),
        host_path_leak_invariant(root),
        navigation_mismatch_invariant(root),
        git_scope_invariant(),
        git_observation_invariant(root),
    ]
}

/// One work's environment, with its capabilities executing in an external environment.
pub struct RemoteEnvironment {
    id: EnvironmentId,
    set: Arc<CapabilitySet>,
    invariants_root: std::path::PathBuf,
    description: EnvironmentDescription,
}

impl RemoteEnvironment {
    /// Connects to the project in the environment: learns what Rust Chip's worker declares there,
    /// where the project is (for the invariants only), and that its test runner works. Fails closed
    /// with the reason.
    pub async fn connect(
        id: EnvironmentId,
        runner: Arc<dyn CommandRunner>,
        worker: WorkerCommand,
    ) -> Result<Self, EnvironmentError> {
        let info = match call(runner.as_ref(), &worker, &Request::Info)
            .await
            .map_err(EnvironmentError::Unavailable)?
        {
            Response::Info(info) => info,
            other => {
                return Err(EnvironmentError::Unavailable(format!(
                    "unexpected answer to an info request: {other:?}"
                )));
            }
        };
        if let Some(why) = info.verifier_error {
            return Err(EnvironmentError::Unavailable(format!(
                "the test runner is not usable in the environment: {why}"
            )));
        }
        let backend = RemoteCapabilityBackend::connect(runner, worker).await?;
        Ok(Self {
            id,
            set: Arc::new(CapabilitySet::new().with(Arc::new(backend))),
            invariants_root: info.root.into(),
            description: EnvironmentDescription {
                verifier_version: info.verifier_version,
                diagnostic: None,
            },
        })
    }
}

impl WorkEnvironment for RemoteEnvironment {
    fn id(&self) -> &EnvironmentId {
        &self.id
    }

    fn capabilities(&self) -> Arc<CapabilitySet> {
        self.set.clone()
    }

    fn observation_invariants(&self) -> Vec<Arc<dyn ObservationInvariant>> {
        project_invariants(&self.invariants_root)
    }

    fn description(&self) -> EnvironmentDescription {
        self.description.clone()
    }
}
