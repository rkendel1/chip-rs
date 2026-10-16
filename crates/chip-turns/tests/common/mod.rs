//! Doubles that sit where the host sits: a scripted model behind chip-core's `Agent`, and an
//! executor that records how often Chip ran something. Nothing here replaces the work loop, the
//! decision boundary, the turn mapping or the session writer.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chip_core::{
    Agent, CapabilityAvailability, CapabilityDescriptor, CapabilityError, CapabilityId,
    CapabilityInput, CapabilityProvider, ExecutionError, ExecutionRequest, ExecutionResult,
    Executor, ModelDecisionBoundary, NoLocalPolicy, WorkGoal, WorkId, WorkSpec,
};
use chip_inference::{FxError, Message, ModelProvider, ModelRequest, ModelResponse, Usage};
use chip_session::{IdSource, Identifier, Kind, NewEvent, SessionWriter};

pub struct SeqIds(pub u128);

impl IdSource for SeqIds {
    fn next_id(&mut self) -> std::io::Result<Identifier> {
        self.0 += 1;
        Ok(Identifier(self.0.to_be_bytes()))
    }
}

pub fn ids(start: u128) -> Box<dyn IdSource> {
    Box::new(SeqIds(start))
}

/// Serves the scripted replies in order, recording every request it was handed.
pub struct ScriptedModel {
    replies: Mutex<VecDeque<String>>,
    pub requests: Mutex<Vec<Vec<String>>>,
    pub calls: AtomicUsize,
    pub before_reply: Mutex<Option<Box<dyn Fn() + Send>>>,
}

impl ScriptedModel {
    pub fn new(replies: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.iter().map(|r| r.to_string()).collect()),
            requests: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
            before_reply: Mutex::new(None),
        })
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    pub fn first_request_text(&self) -> String {
        self.requests.lock().unwrap()[0].join("\n")
    }
}

#[async_trait]
impl ModelProvider for ScriptedModel {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, FxError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let texts: Vec<String> = request
            .messages
            .iter()
            .map(|m: &Message| m.content.clone())
            .collect();
        self.requests.lock().unwrap().push(texts);
        if let Some(hook) = self.before_reply.lock().unwrap().as_ref() {
            hook();
        }
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("the model was called more often than the test scripted");
        Ok(ModelResponse::new("r", reply, Usage::new(10, 5)))
    }
}

pub struct Caps;

#[async_trait]
impl CapabilityProvider for Caps {
    async fn capabilities(&self) -> Result<Vec<CapabilityDescriptor>, CapabilityError> {
        let mut descriptor = CapabilityDescriptor::new(
            CapabilityId::new("compute.selftest")?,
            "selftest",
            "runs the self test",
        );
        descriptor.inputs.push(CapabilityInput {
            name: "n".into(),
            description: String::new(),
            required: false,
        });
        Ok(vec![descriptor])
    }

    async fn availability(&self, _: &CapabilityId) -> CapabilityAvailability {
        CapabilityAvailability::Available
    }
}

pub struct CountingExecutor {
    pub runs: AtomicUsize,
    pub fail: bool,
}

impl CountingExecutor {
    pub fn new(fail: bool) -> Arc<Self> {
        Arc::new(Self {
            runs: AtomicUsize::new(0),
            fail,
        })
    }

    pub fn runs(&self) -> usize {
        self.runs.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Executor for CountingExecutor {
    async fn execute(&self, request: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            return Ok(ExecutionResult::failure(request.id, "self test failed"));
        }
        Ok(ExecutionResult::success(request.id, "self test passed")
            .with_receipt_id("sha256:receipt"))
    }
}

pub fn agent(model: Arc<ScriptedModel>, executor: Arc<CountingExecutor>) -> Agent {
    Agent::new(model)
        .with_capabilities(Arc::new(Caps))
        .with_executor(executor)
        .with_observer(Arc::new(chip_core::ExecutionObserver))
}

pub fn spec(id: WorkId, goal: WorkGoal) -> WorkSpec {
    WorkSpec::new(id, goal)
}

pub const REQUEST_SELFTEST: &str =
    r#"{"decision":"request_capability","capability":"compute.selftest","inputs":{"n":3}}"#;
pub const COMPLETE: &str = r#"{"decision":"complete","summary":"the self test passed"}"#;
pub const BLOCK: &str = r#"{"decision":"block","reason":"cannot proceed"}"#;

pub const STARTED: &str = r#"{"id":"s1","created_at_ms":1,"origin_workspace_root":"/w","workspace_root":"/w","conversation_language":"und-Latn","preferences":{"model":"m","effort":"auto","fast_mode":false}}"#;

/// A Rust-created session under a fresh temp dir.
pub fn rust_session() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("sessions");
    SessionWriter::create(
        &root,
        "s1",
        NewEvent::new(Kind::SessionStarted, 1, STARTED).unwrap(),
        ids(100),
        None,
    )
    .unwrap();
    let dir = root.join("s1");
    (temp, dir)
}

/// A session Zig wrote (recorded from the real binary), copied with the modes the writer requires.
pub fn zig_session(scenario: &str) -> (tempfile::TempDir, PathBuf) {
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../chip-session/tests/fixtures/zig")
        .join(scenario);
    let id = serde_json::from_slice::<serde_json::Value>(
        &fs::read(src.join("PROVENANCE.json")).unwrap(),
    )
    .unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join(&id);
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    for e in fs::read_dir(&src).unwrap() {
        let e = e.unwrap();
        if e.file_name() != "PROVENANCE.json" {
            fs::copy(e.path(), dir.join(e.file_name())).unwrap();
            fs::set_permissions(dir.join(e.file_name()), fs::Permissions::from_mode(0o600))
                .unwrap();
        }
    }
    (temp, dir)
}

pub async fn turn(
    dir: &Path,
    model: &Arc<ScriptedModel>,
    executor: &Arc<CountingExecutor>,
    prompt: &str,
    ids_start: u128,
    crash_at: Option<chip_session::Boundary>,
) -> Result<chip_turns::PersistedTurn, chip_turns::TurnError> {
    let agent = agent(model.clone(), executor.clone());
    chip_turns::run_turn(
        dir,
        chip_turns::TurnEnv {
            agent: &agent,
            policy: &NoLocalPolicy,
            boundary: &ModelDecisionBoundary,
        },
        prompt,
        spec,
        ids(ids_start),
        chip_turns::TurnOptions {
            now_ms: 1_000,
            crash_at,
            ..Default::default()
        },
    )
    .await
}
