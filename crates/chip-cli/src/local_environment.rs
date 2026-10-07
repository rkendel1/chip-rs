//! The local environment: the machine Chip is running on, as an implementation of the environment
//! boundary. It is the default and needs nothing else: no service, no network, no other runtime.
//!
//! It is one project directory with its project, Git and test capabilities, exactly as `chip work`
//! has always used them. There is one such directory, and it is mutable, so the local provider
//! has an isolation capacity of 1: a second work cannot own it while the first does, and the
//! standalone service therefore runs one work at a time. Chip does not clone, branch, lock or
//! sandbox the directory to get around that; an environment that can isolate work (several
//! directories, or an external runtime) is provided through the same [`EnvironmentProvider`]
//! contract.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chip_core::{
    CapabilityAvailability, CapabilityId, CapabilityProvider, CapabilitySet,
    EnvironmentDescription, EnvironmentError, EnvironmentId, EnvironmentProvider,
    ObservationInvariant, WorkEnvironment, WorkId,
};
use chip_pax::{PaxExecutor, ResolvedPax};
use chip_project::{
    PROJECT_LIST, PROJECT_READ, PROJECT_SEARCH, PROJECT_WRITE, ProjectExecutor,
    git_observation_invariant, git_scope_invariant, host_path_leak_invariant,
    navigation_mismatch_invariant, out_of_root_write_invariant, path_escape_invariant,
};
use sha2::{Digest, Sha256};

/// One project directory and the capabilities that operate on it.
pub struct LocalEnvironment {
    id: EnvironmentId,
    root: PathBuf,
    set: Arc<CapabilitySet>,
    description: EnvironmentDescription,
}

impl LocalEnvironment {
    pub fn new(
        id: EnvironmentId,
        root: &Path,
        pax: PaxExecutor,
        description: EnvironmentDescription,
    ) -> Self {
        let set = CapabilitySet::new()
            .with(Arc::new(ProjectExecutor::new(root)))
            .with(Arc::new(pax));
        Self {
            id,
            root: root.to_path_buf(),
            set: Arc::new(set),
            description,
        }
    }
}

impl WorkEnvironment for LocalEnvironment {
    fn id(&self) -> &EnvironmentId {
        &self.id
    }

    fn capabilities(&self) -> Arc<CapabilitySet> {
        self.set.clone()
    }

    fn observation_invariants(&self) -> Vec<Arc<dyn ObservationInvariant>> {
        let root = &self.root;
        vec![
            path_escape_invariant(root),
            out_of_root_write_invariant(root),
            host_path_leak_invariant(root),
            navigation_mismatch_invariant(root),
            git_scope_invariant(),
            git_observation_invariant(root),
        ]
    }

    fn description(&self) -> EnvironmentDescription {
        self.description.clone()
    }
}

/// An opaque id for a directory: a hash, so it names nothing.
pub fn opaque_id(root: &Path) -> EnvironmentId {
    let mut hash = Sha256::new();
    hash.update(root.as_os_str().as_encoded_bytes());
    let digest = hash.finalize();
    let hex: String = digest[..6].iter().map(|b| format!("{b:02x}")).collect();
    EnvironmentId::new(format!("env_{hex}"))
}

/// The one local project directory, leased to one work at a time.
pub struct LocalEnvironmentProvider {
    id: EnvironmentId,
    root: PathBuf,
    description: EnvironmentDescription,
    leased: Mutex<Option<WorkId>>,
}

impl LocalEnvironmentProvider {
    /// Finds the test runner and checks the project capabilities. Fails closed with the reason;
    /// nothing has run when it does.
    pub async fn prepare(root: &Path) -> Result<Self, String> {
        let pax: ResolvedPax = PaxExecutor::new(root)
            .resolve()
            .await
            .map_err(|why| format!("PAX unavailable ({why}); nothing was run"))?;
        let project = ProjectExecutor::new(root);
        for id in [PROJECT_LIST, PROJECT_SEARCH, PROJECT_READ, PROJECT_WRITE] {
            let ready = project.availability(&CapabilityId::new(id).unwrap()).await;
            if !matches!(ready, CapabilityAvailability::Available) {
                return Err(format!("{id} is unavailable; nothing was run"));
            }
        }
        Ok(Self {
            id: opaque_id(root),
            root: root.to_path_buf(),
            description: EnvironmentDescription {
                verifier_version: Some(pax.version.clone()),
                diagnostic: Some(pax.path.display().to_string()),
            },
            leased: Mutex::new(None),
        })
    }
}

#[async_trait::async_trait]
impl EnvironmentProvider for LocalEnvironmentProvider {
    fn isolation_capacity(&self) -> usize {
        1
    }

    async fn acquire(&self, work: &WorkId) -> Result<Arc<dyn WorkEnvironment>, EnvironmentError> {
        let mut leased = self.leased.lock().unwrap_or_else(|e| e.into_inner());
        if leased.is_some() {
            return Err(EnvironmentError::Unavailable(
                "the local environment is one mutable project directory and is in use; concurrent work requires isolated environments".into(),
            ));
        }
        *leased = Some(work.clone());
        // A fresh capability set each time; the environment (and its id) is the same directory.
        Ok(Arc::new(LocalEnvironment::new(
            self.id.clone(),
            &self.root,
            PaxExecutor::new(&self.root),
            self.description.clone(),
        )))
    }

    fn release(&self, work: &WorkId, _environment: &EnvironmentId) {
        let mut leased = self.leased.lock().unwrap_or_else(|e| e.into_inner());
        if leased.as_ref() == Some(work) {
            *leased = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chip_core::Environments;

    fn project(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("chip-localenv-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        dir
    }

    fn provider(dir: &Path) -> LocalEnvironmentProvider {
        LocalEnvironmentProvider {
            id: opaque_id(dir),
            root: dir.to_path_buf(),
            description: EnvironmentDescription::default(),
            leased: Mutex::new(None),
        }
    }

    #[tokio::test]
    async fn the_local_environment_is_one_mutable_directory_owned_by_one_work_at_a_time() {
        let dir = project("lease");
        let envs = Environments::new(Arc::new(provider(&dir)));
        assert_eq!(envs.isolation_capacity(), 1);
        let a = envs.acquire(WorkId::new("A")).await.unwrap();
        let err = envs.acquire(WorkId::new("B")).await.err().unwrap();
        assert!(err.to_string().contains("isolated environments"), "{err}");
        assert!(!err.to_string().contains(dir.to_str().unwrap()));
        drop(a);
        // Sequential reuse of the same environment is fine.
        let b = envs.acquire(WorkId::new("B")).await.unwrap();
        assert_eq!(b.work().as_str(), "B");
    }

    #[test]
    fn the_identity_names_nothing_and_is_stable_per_directory() {
        let (a, b) = (project("id-a"), project("id-b"));
        let id = opaque_id(&a);
        assert_eq!(id, opaque_id(&a));
        assert_ne!(id, opaque_id(&b));
        assert!(id.as_str().starts_with("env_"));
        assert!(!id.as_str().contains("chip-localenv") && !id.as_str().contains('/'));
    }

    #[tokio::test]
    async fn the_environment_declares_the_existing_capabilities_and_invariants() {
        use chip_core::CapabilityProvider;
        let dir = project("caps");
        let env = LocalEnvironment::new(
            opaque_id(&dir),
            &dir,
            PaxExecutor::new(&dir),
            EnvironmentDescription::default(),
        );
        let declared: Vec<String> = env
            .capabilities()
            .capabilities()
            .await
            .unwrap()
            .iter()
            .map(|d| d.id.to_string())
            .collect();
        for id in [
            "project.read",
            "project.write",
            "project.list",
            "project.search",
            "project.git.status",
            "project.git.diff",
            "project.git.diff_stat",
            "project.git.log",
            "pax.test",
        ] {
            assert!(declared.iter().any(|d| d == id), "{id} in {declared:?}");
        }
        assert_eq!(env.observation_invariants().len(), 6);
    }
}
