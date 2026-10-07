//! The environment boundary: where a work item's capabilities actually operate.
//!
//! Chip decides, validates and evaluates; an environment performs. A [`WorkEnvironment`] is the
//! contract between the two: it declares and executes the capabilities (the existing
//! [`CapabilityBackend`](crate::CapabilityBackend) boundary, presented as a [`CapabilitySet`]),
//! supplies the observation invariants that fit its reality, and has an opaque identity. Chip knows
//! nothing else about it: not whether it is the local machine or something hosted elsewhere, not
//! where it is, not how it is isolated.
//!
//! Each work item acquires exactly one environment from an [`EnvironmentProvider`] and keeps it for
//! its whole trajectory. [`Environments`] enforces the ownership rule at this boundary whatever the
//! provider does: an environment is mutable, so it has at most one owning work at a time, and a
//! provider that hands the same one to a second work is refused, not trusted. Acquisition that
//! fails is a failure: there is no fallback to another or shared environment.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::{CapabilitySet, ObservationInvariant, WorkId};

/// Opaque identity of an environment. It carries no meaning: no path, host, container or machine.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EnvironmentId(String);

impl EnvironmentId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EnvironmentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvironmentError {
    /// No environment could be provided. The reason must not name a host path.
    Unavailable(String),
    /// The environment is owned by another work. Mutable environments are never shared.
    AlreadyOwned {
        environment: EnvironmentId,
        owner: WorkId,
    },
    /// The provider has no more isolated environments than it declared.
    AtCapacity { capacity: usize },
}

impl fmt::Display for EnvironmentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(reason) => write!(f, "environment unavailable: {reason}"),
            Self::AlreadyOwned { environment, owner } => write!(
                f,
                "environment {environment} is already owned by work {owner}; a mutable environment is never shared"
            ),
            Self::AtCapacity { capacity } => write!(
                f,
                "all {capacity} isolated environment(s) are in use; concurrent work requires isolated environments"
            ),
        }
    }
}

impl std::error::Error for EnvironmentError {}

/// What an operator may be told about an environment's verifier. Never model-visible.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EnvironmentDescription {
    /// The version of the tool that runs the project's tests, if it reports one.
    pub verifier_version: Option<String>,
    /// Operator-facing detail (for a local environment, where the verifier is). Not model-visible
    /// and never part of the service's responses.
    pub diagnostic: Option<String>,
}

/// Where one work item's capabilities operate. Implementations own the actual operations.
pub trait WorkEnvironment: Send + Sync {
    fn id(&self) -> &EnvironmentId;

    /// The capabilities this environment declares and executes. The same set serves the whole
    /// trajectory: the agent never sees two worlds.
    fn capabilities(&self) -> Arc<CapabilitySet>;

    /// Invariants over the observations this environment produces (for example, that a filesystem
    /// observation names nothing outside its root).
    fn observation_invariants(&self) -> Vec<Arc<dyn ObservationInvariant>>;

    fn description(&self) -> EnvironmentDescription;
}

/// Where environments come from.
#[async_trait::async_trait]
pub trait EnvironmentProvider: Send + Sync {
    /// How many environments may be owned at once. A provider with one shared mutable workspace
    /// says 1.
    fn isolation_capacity(&self) -> usize;

    /// An environment for `work`. Fails rather than substituting something else.
    async fn acquire(&self, work: &WorkId) -> Result<Arc<dyn WorkEnvironment>, EnvironmentError>;

    /// The work no longer owns `environment`. Called once for every environment [`Environments`]
    /// accepted, including when the work ended abnormally.
    fn release(&self, work: &WorkId, environment: &EnvironmentId);
}

type Owners = Arc<Mutex<HashMap<EnvironmentId, WorkId>>>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The ownership rule, enforced here for every provider.
pub struct Environments {
    provider: Arc<dyn EnvironmentProvider>,
    owners: Owners,
}

impl Environments {
    pub fn new(provider: Arc<dyn EnvironmentProvider>) -> Self {
        Self {
            provider,
            owners: Arc::default(),
        }
    }

    pub fn isolation_capacity(&self) -> usize {
        self.provider.isolation_capacity()
    }

    pub async fn acquire(&self, work: WorkId) -> Result<OwnedEnvironment, EnvironmentError> {
        let capacity = self.provider.isolation_capacity();
        if lock(&self.owners).len() >= capacity {
            return Err(EnvironmentError::AtCapacity { capacity });
        }
        let environment = self.provider.acquire(&work).await?;
        let id = environment.id().clone();
        let mut owners = lock(&self.owners);
        if let Some(owner) = owners.get(&id) {
            return Err(EnvironmentError::AlreadyOwned {
                environment: id,
                owner: owner.clone(),
            });
        }
        owners.insert(id, work.clone());
        drop(owners);
        Ok(OwnedEnvironment {
            environment,
            work,
            owners: self.owners.clone(),
            provider: self.provider.clone(),
        })
    }
}

/// One work's environment, for the whole trajectory. Dropping it releases the environment, so a
/// work that ends by panic releases it too.
pub struct OwnedEnvironment {
    environment: Arc<dyn WorkEnvironment>,
    work: WorkId,
    owners: Owners,
    provider: Arc<dyn EnvironmentProvider>,
}

impl OwnedEnvironment {
    pub fn environment(&self) -> &dyn WorkEnvironment {
        self.environment.as_ref()
    }

    pub fn id(&self) -> &EnvironmentId {
        self.environment.id()
    }

    pub fn work(&self) -> &WorkId {
        &self.work
    }
}

impl Drop for OwnedEnvironment {
    fn drop(&mut self) {
        lock(&self.owners).remove(self.environment.id());
        self.provider.release(&self.work, self.environment.id());
    }
}
