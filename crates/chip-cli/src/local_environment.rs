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
use chip_pax::PaxExecutor;
use chip_project::{PROJECT_LIST, PROJECT_READ, PROJECT_SEARCH, PROJECT_WRITE, ProjectExecutor};
use sha2::{Digest, Sha256};

/// One project directory and the capabilities that operate on it.
pub struct LocalEnvironment {
    id: EnvironmentId,
    root: PathBuf,
    set: Arc<CapabilitySet>,
    /// The same executor the capability set holds (clones share what it has verified), kept to
    /// report the PAX that was actually used. Reading it starts nothing.
    pax: PaxExecutor,
    description: EnvironmentDescription,
}

impl LocalEnvironment {
    pub fn new(
        id: EnvironmentId,
        root: &Path,
        pax: PaxExecutor,
        description: EnvironmentDescription,
    ) -> Self {
        let set = chip_remote_env::project_capability_set(root, pax.clone());
        Self {
            id,
            root: root.to_path_buf(),
            set: Arc::new(set),
            pax,
            description,
        }
    }
}

impl LocalEnvironment {
    /// Offers `project.observe` as well. Explicit: the default environment does not.
    pub fn with_project_observe(mut self) -> Self {
        self.set = Arc::new(chip_remote_env::project_capability_set_observing(
            &self.root,
            self.pax.clone(),
        ));
        self
    }
}

/// The environment variable that opts a work in to `project.observe`.
pub const PROJECT_OBSERVE_ENV: &str = "CHIP_ENABLE_PROJECT_OBSERVE";

/// Whether `project.observe` is offered: off unless the variable is exactly `true`. Anything other than
/// `true` or `false` is a configuration error, never a guess.
pub fn project_observe_from_env(get: impl Fn(&str) -> Option<String>) -> Result<bool, String> {
    match get(PROJECT_OBSERVE_ENV).as_deref().map(str::trim) {
        None | Some("") | Some("false") => Ok(false),
        Some("true") => Ok(true),
        Some(_) => Err(format!("{PROJECT_OBSERVE_ENV} must be `true` or `false`")),
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
        chip_remote_env::project_invariants(&self.root)
    }

    /// The PAX version is known only if the work needed PAX; a work that never ran a test did not
    /// start it and does not report one.
    fn description(&self) -> EnvironmentDescription {
        let mut description = self.description.clone();
        if let Some(pax) = self.pax.resolved() {
            description.verifier_version = Some(pax.version);
            description.diagnostic = Some(pax.path.display().to_string());
        }
        description
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
    observe: bool,
}

impl LocalEnvironmentProvider {
    /// Checks that a test runner is in place and the project capabilities are usable. Fails closed
    /// with the reason; nothing has run when it does. PAX is only *located* here (a filesystem
    /// lookup): it is not started, because a work that never runs a test never needs it. That it is
    /// PAX, and new enough, is verified when a `pax.test` request first needs it.
    pub async fn prepare(root: &Path) -> Result<Self, String> {
        let pax_path = PaxExecutor::new(root)
            .locate()
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
                verifier_version: None,
                diagnostic: Some(pax_path.display().to_string()),
            },
            leased: Mutex::new(None),
            observe: false,
        })
    }

    /// Whether the environments this provider hands out offer `project.observe`. Off unless asked for.
    pub fn with_project_observe(mut self, offered: bool) -> Self {
        self.observe = offered;
        self
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
        let environment = LocalEnvironment::new(
            self.id.clone(),
            &self.root,
            PaxExecutor::new(&self.root),
            self.description.clone(),
        );
        Ok(Arc::new(if self.observe {
            environment.with_project_observe()
        } else {
            environment
        }))
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
            observe: false,
        }
    }

    fn get(vars: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| {
            vars.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn project_observe_is_off_unless_the_variable_is_exactly_true() {
        assert_eq!(project_observe_from_env(get(&[])), Ok(false));
        assert_eq!(
            project_observe_from_env(get(&[(PROJECT_OBSERVE_ENV, "")])),
            Ok(false)
        );
        assert_eq!(
            project_observe_from_env(get(&[(PROJECT_OBSERVE_ENV, "false")])),
            Ok(false)
        );
        assert_eq!(
            project_observe_from_env(get(&[(PROJECT_OBSERVE_ENV, "true")])),
            Ok(true)
        );
        for bad in ["1", "yes", "TRUE", "on", "observe"] {
            let vars: &'static [(&str, &str)] = Box::leak(Box::new([(PROJECT_OBSERVE_ENV, bad)]));
            assert!(
                project_observe_from_env(get(vars)).is_err(),
                "{bad:?} must be a configuration error, not a guess"
            );
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
