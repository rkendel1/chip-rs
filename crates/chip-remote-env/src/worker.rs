//! The worker: Rust Chip's own executors, run in the environment against the project there.
//!
//! `chip-cli capability-exec --root <dir>` reads one request from `CHIP_CAPABILITY_REQUEST`, runs
//! it with the same `ProjectExecutor` and `PaxExecutor` that `chip work` uses locally, and prints
//! one response. It decides nothing: Chip has already validated and authorized the request, and
//! the executors re-check their own inputs exactly as they do locally.

use std::path::Path;

use chip_core::{CapabilityId, CapabilityProvider, ExecutionId, ExecutionRequest, Executor};
use chip_pax::PaxExecutor;

use crate::protocol::{Info, Request, Response, WORKER_ENV};

/// Exit status of a worker that could not produce a response at all.
pub const EXIT_PROTOCOL: i32 = 2;

fn set(root: &Path) -> chip_core::CapabilitySet {
    crate::project_capability_set(root, PaxExecutor::new(root))
}

/// Answers one request.
pub async fn handle(root: &Path, request: Request) -> Response {
    // The executors (and the test runner they start) resolve the project against their own working
    // directory, so the project is addressed absolutely, wherever the command was started.
    let absolute = std::fs::canonicalize(root);
    let root = absolute.as_deref().unwrap_or(root);
    let set = set(root);
    match request {
        Request::Info => {
            let pax = PaxExecutor::new(root).resolve().await;
            Response::Info(Info {
                root: root.to_string_lossy().into_owned(),
                verifier_version: pax.as_ref().ok().map(|p| p.version.clone()),
                verifier_error: pax.err().map(|e| e.to_string()),
            })
        }
        Request::Capabilities => {
            Response::Capabilities(set.capabilities().await.unwrap_or_default())
        }
        Request::Availability { capability } => {
            Response::Availability(match CapabilityId::new(capability) {
                Ok(id) => set.availability(&id).await,
                Err(e) => chip_core::CapabilityAvailability::Unavailable(format!("{e:?}")),
            })
        }
        Request::Validate { capability, inputs } => {
            Response::Validated(match CapabilityId::new(capability) {
                Ok(id) => set.validate_inputs(&id, &inputs).await,
                Err(e) => Err(e),
            })
        }
        Request::Execute {
            execution_id,
            capability,
            inputs,
        } => {
            let request = ExecutionRequest::new(ExecutionId::new(execution_id), capability)
                .with_inputs(inputs);
            Response::Executed(set.execute(request).await)
        }
    }
}

/// The command: request from the environment, response on stdout. Returns the exit status.
pub async fn run(root: &Path) -> i32 {
    let Ok(encoded) = std::env::var(WORKER_ENV) else {
        eprintln!("error: {WORKER_ENV} is not set");
        return EXIT_PROTOCOL;
    };
    let request = match Request::decode(&encoded) {
        Ok(request) => request,
        Err(why) => {
            eprintln!("error: {why}");
            return EXIT_PROTOCOL;
        }
    };
    println!("{}", handle(root, request).await.encode());
    0
}
