//! Driver for the cross-implementation check (`docs/rebuild/tools/verify_rust_session_resume.py` in
//! rkendel1/fx): a real Chip turn over a session directory, with a scripted model so it is offline
//! and deterministic. The work loop, decision boundary, turn mapping and session writer are the
//! real ones; only the model's replies and the capability's output are scripted.
//!
//!   turn_tool run <session_dir> <replies.json> <prompt>   # env CHIP_SESSION_CRASH=<boundary> to die mid-write
//!   turn_tool history <session_dir>                       # JSON summary of the committed history

use std::collections::VecDeque;
use std::path::Path;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chip_core::{
    Agent, CapabilityAvailability, CapabilityDescriptor, CapabilityError, CapabilityId,
    CapabilityInput, CapabilityProvider, ExecutionError, ExecutionRequest, ExecutionResult,
    Executor, ModelDecisionBoundary, NoLocalPolicy, WorkGoal, WorkId, WorkSpec,
};
use chip_inference::{FxError, ModelProvider, ModelRequest, ModelResponse, Usage};
use chip_session::{Boundary, HistoryTurn, OsIds, read_session, reduce_history};
use chip_turns::{Persistence, TurnEnv, TurnError, TurnOptions, run_turn};

struct Scripted(Mutex<VecDeque<String>>);

#[async_trait]
impl ModelProvider for Scripted {
    async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse, FxError> {
        let reply = self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .expect("the script ran out of replies");
        Ok(ModelResponse::new("scripted", reply, Usage::new(10, 5)))
    }
}

struct Caps;

#[async_trait]
impl CapabilityProvider for Caps {
    async fn capabilities(&self) -> Result<Vec<CapabilityDescriptor>, CapabilityError> {
        let mut d = CapabilityDescriptor::new(
            CapabilityId::new("compute.selftest")?,
            "selftest",
            "runs the self test",
        );
        d.inputs.push(CapabilityInput {
            name: "n".into(),
            description: String::new(),
            required: false,
        });
        Ok(vec![d])
    }

    async fn availability(&self, _: &CapabilityId) -> CapabilityAvailability {
        CapabilityAvailability::Available
    }
}

struct SelfTest;

#[async_trait]
impl Executor for SelfTest {
    async fn execute(&self, request: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        Ok(ExecutionResult::success(request.id, "self test passed"))
    }
}

fn boundary(name: &str) -> Option<Boundary> {
    Some(match name {
        "after_event_append" => Boundary::AfterEventAppend,
        "after_event_sync" => Boundary::AfterEventSync,
        "after_intent_sync" => Boundary::AfterIntentSync,
        "after_watermark_rename" => Boundary::AfterWatermarkRename,
        "after_intent_remove" => Boundary::AfterIntentRemove,
        _ => return None,
    })
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    match (args.get(1).map(String::as_str), args.len()) {
        (Some("run"), 5) => run(&args[2], &args[3], &args[4]).await,
        (Some("history"), 3) => history(&args[2]),
        _ => {
            eprintln!(
                "usage: turn_tool run <session_dir> <replies.json> <prompt> | history <session_dir>"
            );
            ExitCode::from(2)
        }
    }
}

async fn run(dir: &str, replies: &str, prompt: &str) -> ExitCode {
    let replies: Vec<String> =
        serde_json::from_slice(&std::fs::read(replies).expect("replies file"))
            .expect("replies are a JSON array of strings");
    let agent = Agent::new(Arc::new(Scripted(Mutex::new(replies.into()))))
        .with_capabilities(Arc::new(Caps))
        .with_executor(Arc::new(SelfTest))
        .with_observer(Arc::new(chip_core::ExecutionObserver));
    let crash_at = std::env::var("CHIP_SESSION_CRASH")
        .ok()
        .and_then(|name| boundary(&name));
    let options = TurnOptions {
        crash_at,
        ..Default::default()
    };
    let spec = |id: WorkId, goal: WorkGoal| WorkSpec::new(id, goal);
    match run_turn(
        Path::new(dir),
        TurnEnv {
            agent: &agent,
            policy: &NoLocalPolicy,
            boundary: &ModelDecisionBoundary,
        },
        prompt,
        spec,
        Box::new(OsIds),
        options,
    )
    .await
    {
        Ok(done) => {
            let (state, detail) = match &done.persistence {
                Persistence::Committed(p) => ("committed", format!("{}", p.through_seq)),
                Persistence::NotCommitted(why) => ("not_committed", why.clone()),
                Persistence::Uncertain(why) => ("uncertain", why.clone()),
            };
            println!(
                "{}",
                serde_json::json!({"outcome": format!("{:?}", done.report.outcome.terminal_state()), "persistence": state, "detail": detail})
            );
            if state == "committed" {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(TurnError::SimulatedCrash(at)) => {
            eprintln!("simulated crash at {at}");
            ExitCode::from(3)
        }
        Err(other) => {
            eprintln!("error: {other}");
            ExitCode::FAILURE
        }
    }
}

fn history(dir: &str) -> ExitCode {
    let log = match read_session(Path::new(dir)) {
        Ok(log) => log,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };
    let history = match reduce_history(&log.frames) {
        Ok(history) => history,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };
    let turns: Vec<_> = history
        .turns
        .iter()
        .map(|t| {
            let (kind, answer) = match t.turn() {
                HistoryTurn::Assistant { assistant, .. } => ("assistant", Some(assistant.clone())),
                HistoryTurn::Interrupted { .. } => ("interrupted", None),
            };
            serde_json::json!({"seq": t.seq, "event_id": t.event_id.to_string(), "kind": kind, "user": t.turn().user().text, "assistant": answer, "tool_calls": t.turn().execution().tool_steps.iter().flat_map(|s| s.tool_calls.iter().map(|c| c.name.clone())).collect::<Vec<_>>()})
        })
        .collect();
    let events: Vec<_> = log.frames.iter().map(|f| serde_json::json!({"seq": f.seq, "event_id": f.event_id.to_string(), "kind": format!("{:?}", f.kind)})).collect();
    println!(
        "{}",
        serde_json::json!({"turns": turns, "events": events, "pending_recovery": history.pending_recovery, "uncommitted_tail_bytes": log.uncommitted_tail_bytes, "through_seq": log.position.through_seq})
    );
    ExitCode::SUCCESS
}
