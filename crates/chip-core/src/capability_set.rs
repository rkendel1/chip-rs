//! A set of capability backends presented to the agent as one provider and one executor.
//!
//! The agent takes a single provider and a single executor. A `CapabilitySet` lets several
//! backends (PAX, project files, Compute) each own their capabilities without any of them knowing
//! about the others. Routing is by declared capability id and nothing else: a request is executed
//! by the backend that declared the id, or by none. There is no fallback to another backend, and
//! two backends declaring the same id is a configuration error that fails closed.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::{
    CapabilityAvailability, CapabilityDescriptor, CapabilityError, CapabilityId,
    CapabilityProvider, ExecutionError, ExecutionRequest, ExecutionResult, Executor, InputValue,
};

/// A backend that both declares capabilities and executes them.
pub trait CapabilityBackend: CapabilityProvider + Executor {}

impl<T: CapabilityProvider + Executor> CapabilityBackend for T {}

#[derive(Default, Clone)]
pub struct CapabilitySet {
    backends: Vec<Arc<dyn CapabilityBackend>>,
}

impl CapabilitySet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, backend: Arc<dyn CapabilityBackend>) -> Self {
        self.backends.push(backend);
        self
    }

    /// The backend that declares `id`, if exactly one does.
    async fn owner(
        &self,
        id: &str,
    ) -> Result<Option<&Arc<dyn CapabilityBackend>>, CapabilityError> {
        let mut found = None;
        for backend in &self.backends {
            if backend
                .capabilities()
                .await?
                .iter()
                .any(|d| d.id.as_str() == id)
            {
                if found.is_some() {
                    return Err(CapabilityError::Unavailable(format!(
                        "capability {id} is declared by more than one backend"
                    )));
                }
                found = Some(backend);
            }
        }
        Ok(found)
    }
}

#[async_trait::async_trait]
impl CapabilityProvider for CapabilitySet {
    async fn capabilities(&self) -> Result<Vec<CapabilityDescriptor>, CapabilityError> {
        let mut all: Vec<CapabilityDescriptor> = Vec::new();
        for backend in &self.backends {
            for descriptor in backend.capabilities().await? {
                if all.iter().any(|d| d.id == descriptor.id) {
                    return Err(CapabilityError::Unavailable(format!(
                        "capability {} is declared by more than one backend",
                        descriptor.id
                    )));
                }
                all.push(descriptor);
            }
        }
        Ok(all)
    }

    async fn availability(&self, id: &CapabilityId) -> CapabilityAvailability {
        match self.owner(id.as_str()).await {
            Ok(Some(backend)) => backend.availability(id).await,
            Ok(None) => CapabilityAvailability::Unavailable(format!("{id} is not declared")),
            Err(e) => CapabilityAvailability::Misconfigured(e.to_string()),
        }
    }

    async fn validate_inputs(
        &self,
        id: &CapabilityId,
        inputs: &BTreeMap<String, InputValue>,
    ) -> Result<(), CapabilityError> {
        match self.owner(id.as_str()).await? {
            Some(backend) => backend.validate_inputs(id, inputs).await,
            None => Err(CapabilityError::Unknown(id.to_string())),
        }
    }
}

#[async_trait::async_trait]
impl Executor for CapabilitySet {
    async fn execute(&self, request: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        match self.owner(&request.intent).await {
            Ok(Some(backend)) => backend.execute(request).await,
            Ok(None) => Err(ExecutionError::InvalidRequest(format!(
                "'{}' is not a capability any backend declares",
                request.intent
            ))),
            Err(e) => Err(ExecutionError::ExecutorUnavailable(e.to_string())),
        }
    }
}
