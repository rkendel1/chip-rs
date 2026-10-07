//! The environment boundary as a contract. A provider from any other runtime is held to exactly
//! this: these tests use fake environments and name no particular machine, host or product.

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use chip_core::{
    CapabilitySet, EnvironmentDescription, EnvironmentError, EnvironmentId, EnvironmentProvider,
    Environments, ObservationInvariant, WorkEnvironment, WorkId,
};

struct Fake(EnvironmentId);

impl WorkEnvironment for Fake {
    fn id(&self) -> &EnvironmentId {
        &self.0
    }
    fn capabilities(&self) -> Arc<CapabilitySet> {
        Arc::new(CapabilitySet::new())
    }
    fn observation_invariants(&self) -> Vec<Arc<dyn ObservationInvariant>> {
        Vec::new()
    }
    fn description(&self) -> EnvironmentDescription {
        EnvironmentDescription::default()
    }
}

/// Hands out `ids` in order, repeating the last (a provider that does not isolate), and counts.
struct Provider {
    ids: Vec<&'static str>,
    capacity: usize,
    handed: AtomicUsize,
    released: Mutex<Vec<(String, String)>>,
    fail: bool,
}

impl Provider {
    fn new(ids: &[&'static str], capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            ids: ids.to_vec(),
            capacity,
            handed: AtomicUsize::new(0),
            released: Mutex::new(Vec::new()),
            fail: false,
        })
    }
}

#[async_trait::async_trait]
impl EnvironmentProvider for Provider {
    fn isolation_capacity(&self) -> usize {
        self.capacity
    }
    async fn acquire(&self, _work: &WorkId) -> Result<Arc<dyn WorkEnvironment>, EnvironmentError> {
        if self.fail {
            return Err(EnvironmentError::Unavailable("none provisioned".into()));
        }
        let n = self.handed.fetch_add(1, Ordering::SeqCst);
        let id = self.ids[n.min(self.ids.len() - 1)];
        Ok(Arc::new(Fake(EnvironmentId::new(id))))
    }
    fn release(&self, work: &WorkId, environment: &EnvironmentId) {
        self.released
            .lock()
            .unwrap()
            .push((work.to_string(), environment.to_string()));
    }
}

fn work(id: &str) -> WorkId {
    WorkId::new(id)
}

#[tokio::test]
async fn isolated_environments_are_owned_by_one_work_each() {
    let provider = Provider::new(&["env-a", "env-b"], 2);
    let envs = Environments::new(provider.clone());
    let a = envs.acquire(work("A")).await.unwrap();
    let b = envs.acquire(work("B")).await.unwrap();
    assert_eq!((a.id().as_str(), b.id().as_str()), ("env-a", "env-b"));
    assert_ne!(a.id(), b.id());
    assert_eq!((a.work().as_str(), b.work().as_str()), ("A", "B"));
}

#[tokio::test]
async fn a_provider_that_shares_one_mutable_environment_is_refused() {
    // Both works are handed environment X while both are alive: the boundary refuses the second.
    let provider = Provider::new(&["env-x"], 4);
    let envs = Environments::new(provider.clone());
    let a = envs.acquire(work("A")).await.unwrap();
    let refused = envs.acquire(work("B")).await.err().unwrap();
    assert_eq!(
        refused,
        EnvironmentError::AlreadyOwned {
            environment: EnvironmentId::new("env-x"),
            owner: work("A"),
        }
    );
    assert!(refused.to_string().contains("never shared"));
    // The refusal did not disturb the owner, and was not a release of its environment.
    assert_eq!(a.id().as_str(), "env-x");
    assert!(provider.released.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_environment_can_be_reused_once_it_is_released() {
    let provider = Provider::new(&["env-x"], 1);
    let envs = Environments::new(provider.clone());
    let a = envs.acquire(work("A")).await.unwrap();
    drop(a);
    assert_eq!(
        provider.released.lock().unwrap().clone(),
        [("A".to_string(), "env-x".to_string())]
    );
    let b = envs.acquire(work("B")).await.unwrap();
    assert_eq!(b.id().as_str(), "env-x");
    assert_eq!(b.work().as_str(), "B");
}

#[tokio::test]
async fn acquisition_past_the_providers_capacity_fails_closed() {
    let provider = Provider::new(&["env-a", "env-b"], 1);
    let envs = Environments::new(provider.clone());
    let _a = envs.acquire(work("A")).await.unwrap();
    assert_eq!(
        envs.acquire(work("B")).await.err().unwrap(),
        EnvironmentError::AtCapacity { capacity: 1 }
    );
    assert_eq!(
        provider.handed.load(Ordering::SeqCst),
        1,
        "the provider was not asked again"
    );
}

#[tokio::test]
async fn a_failed_acquisition_is_an_error_and_owns_nothing() {
    let mut p = Provider::new(&["env-a"], 2);
    Arc::get_mut(&mut p).unwrap().fail = true;
    let envs = Environments::new(p.clone());
    let err = envs.acquire(work("A")).await.err().unwrap();
    assert_eq!(
        err,
        EnvironmentError::Unavailable("none provisioned".into())
    );
    assert!(p.released.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_environment_is_released_when_its_work_ends_by_panic() {
    let provider = Provider::new(&["env-x"], 1);
    let envs = Arc::new(Environments::new(provider.clone()));
    let e = envs.clone();
    let task = tokio::spawn(async move {
        let _owned = e.acquire(work("A")).await.unwrap();
        panic!("the work failed");
    });
    assert!(task.await.unwrap_err().is_panic());
    assert_eq!(provider.released.lock().unwrap().len(), 1);
    assert!(
        envs.acquire(work("B")).await.is_ok(),
        "the environment is free again"
    );
}

#[tokio::test]
async fn concurrent_acquisition_never_yields_the_same_environment_twice() {
    let provider = Provider::new(&["env-1", "env-2", "env-3", "env-4"], 4);
    let envs = Arc::new(Environments::new(provider));
    let tasks: Vec<_> = (0..4)
        .map(|i| {
            let envs = envs.clone();
            tokio::spawn(async move { envs.acquire(work(&format!("W{i}"))).await })
        })
        .collect();
    let mut held = Vec::new();
    for t in tasks {
        held.push(t.await.unwrap().unwrap());
    }
    let ids: HashSet<_> = held.iter().map(|o| o.id().clone()).collect();
    assert_eq!(ids.len(), 4);
}

#[test]
fn an_environment_identity_is_opaque() {
    let id = EnvironmentId::new("env_0123abcd");
    assert_eq!(id.to_string(), "env_0123abcd");
}
