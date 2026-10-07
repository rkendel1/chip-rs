mod benchmark;
mod corpus_eval;
mod decision_state_cmd;
mod graph_cmd;
mod laya_eval;
mod live_benchmark;
mod local_model_bench;
mod native;
mod wasm_decision_bench;
mod work_demo;

use std::sync::Arc;

use chip_core::{
    Agent, AgentDecision, AgentError, Assessment, Capability, CapabilityAvailability,
    CapabilityDescriptor, CapabilityError, CapabilityId, CapabilityProvider, CapabilityRequest,
    DecisionBoundary, DecisionError, DecisionInput, EvidenceOutcome, EvidenceState, ExecutionError,
    ExecutionId, ExecutionObserver, ExecutionRequest, ExecutionResult, Executor,
    LocalReasoningResult, ScriptedDecision, StateToken, TestExecutor, TestLocalReasoner, Turn,
};
use fx_core::{FxError, ModelProvider, ModelRequest, ModelResponse, Secret, Usage};
use fx_provider_http::{HttpProvider, HttpProviderConfig, PROVIDER_OPENAI_COMPATIBLE};

#[derive(Default)]
struct TestModelProvider;

#[async_trait::async_trait]
impl ModelProvider for TestModelProvider {
    async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse, FxError> {
        Ok(ModelResponse::new(
            "test-response",
            "Hello from the test provider.",
            Usage::new(4, 6),
        ))
    }
}

/// Deterministic model for the bounded-cycle demo: it reports whether its request
/// carried an observation.
struct CycleModel;

#[async_trait::async_trait]
impl ModelProvider for CycleModel {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, FxError> {
        let saw_completion = request
            .messages
            .iter()
            .any(|m| m.content.contains("kind: execution.completed"));
        let output = if saw_completion {
            "I was told the execution completed."
        } else {
            "I would like to run the capability."
        };
        Ok(ModelResponse::new(
            "cycle-response",
            output,
            Usage::new(1, 1),
        ))
    }
}

/// First decision requests a capability; the second responds.
struct CycleDecisions(std::sync::atomic::AtomicUsize, &'static str);

impl DecisionBoundary for CycleDecisions {
    fn decide(
        &self,
        response: &ModelResponse,
        capabilities: &[Capability],
    ) -> Result<AgentDecision, DecisionError> {
        let input = if self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            DecisionInput::RequestCapability {
                execution_id: ExecutionId::new("cycle-1"),
                capability_id: self.1.into(),
                inputs: Default::default(),
            }
        } else {
            DecisionInput::Respond
        };
        ScriptedDecision::new(input).decide(response, capabilities)
    }
}

/// Deterministic model for the workload proof: counts calls and reports only
/// what the observation in its request says.
struct ProofModel(Arc<std::sync::atomic::AtomicUsize>);

#[async_trait::async_trait]
impl ModelProvider for ProofModel {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, FxError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let has = |needle: &str| request.messages.iter().any(|m| m.content.contains(needle));
        let output = if has("kind: execution.completed") {
            "observed the execution result"
        } else if has("kind: execution.failed") {
            "observed that the execution failed"
        } else {
            "request the self-test capability"
        };
        Ok(ModelResponse::new(
            "proof-response",
            output,
            Usage::new(1, 1),
        ))
    }
}

/// Counts calls to any executor.
struct CountingExecutor {
    inner: Arc<dyn Executor>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl Executor for CountingExecutor {
    async fn execute(&self, request: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.execute(request).await
    }
}

enum ProofOutcome {
    Completed,
    Skipped(String),
    Failed(String),
}

/// The bounded workload proof: every step is an explicit call by this function,
/// which is the caller. Used with a test executor and with real Compute.
async fn run_workload_proof(
    title: &str,
    capability: &'static str,
    capabilities: Arc<dyn CapabilityProvider>,
    executor: Arc<dyn Executor>,
    skippable: bool,
) -> ProofOutcome {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let model_calls = Arc::new(AtomicUsize::new(0));
    let executions = Arc::new(AtomicUsize::new(0));
    let agent = Agent::new(Arc::new(ProofModel(model_calls.clone())))
        .with_decision_boundary(Arc::new(CycleDecisions(Default::default(), capability)))
        .with_capabilities(capabilities)
        .with_executor(Arc::new(CountingExecutor {
            inner: executor,
            calls: executions.clone(),
        }))
        .with_observer(Arc::new(ExecutionObserver));
    let fail = |e: &dyn std::fmt::Display| ProofOutcome::Failed(e.to_string());

    // Availability is read from the capability contract; nothing executes here.
    if skippable {
        if let Ok(found) = agent.discover_capabilities().await.result {
            for capability in found {
                if let CapabilityAvailability::Unavailable(reason)
                | CapabilityAvailability::Misconfigured(reason) = capability.availability
                {
                    return ProofOutcome::Skipped(reason);
                }
            }
        }
    }

    println!("{title}\n");
    let first = match agent
        .decide(Turn::new(
            "Perform a bounded self-test and report what happened",
        ))
        .await
    {
        Ok(report) => report,
        Err(e) => return fail(&e),
    };
    let Ok(AgentDecision::RequestCapability(request)) = first.decision else {
        return ProofOutcome::Failed("turn 1 did not request a capability".into());
    };
    println!("Turn 1:\n  Decision: request {}\n", request.capability_id);

    let result = match agent.execute_capability(&request).await {
        Ok(report) => match report.result {
            Ok(result) => result,
            Err(ExecutionError::ExecutorUnavailable(reason)) if skippable => {
                return ProofOutcome::Skipped(reason);
            }
            Err(e) => return fail(&e),
        },
        Err(e) => return fail(&e),
    };
    println!(
        "Execution:\n  Status: {}\n  Receipt: {}\n",
        format!("{:?}", result.status).to_lowercase(),
        result.receipt_id.as_deref().unwrap_or("none")
    );

    let observation = match agent.observe(&result) {
        Ok(o) => o,
        Err(e) => return fail(&e),
    };
    println!(
        "Observation:\n  Kind: {}\n  Status: {}\n",
        observation.kind.as_str(),
        format!("{:?}", observation.status).to_lowercase()
    );

    let second = match agent
        .decide_with_observations(
            Turn::new("What actually happened?"),
            std::slice::from_ref(&observation),
        )
        .await
    {
        Ok(report) => report,
        Err(e) => return fail(&e),
    };
    match second.decision {
        Ok(AgentDecision::Respond(response)) => {
            println!("Turn 2:\n  Response: {}\n", response.output)
        }
        Ok(AgentDecision::RequestCapability(_)) => {
            println!("Turn 2:\n  Capability requested (not executed)\n")
        }
        Err(e) => return fail(&e),
    }

    println!(
        "Proof:\n  Model calls: {}\n  Executions: {}\n  Observations: 1\n  Automatic follow-ups: 0\n",
        model_calls.load(Ordering::SeqCst),
        executions.load(Ordering::SeqCst)
    );
    ProofOutcome::Completed
}

/// Adds a fixed receipt id to results from a wrapped executor (deterministic demo).
struct FixedReceipt(Arc<dyn Executor>);

#[async_trait::async_trait]
impl Executor for FixedReceipt {
    async fn execute(&self, request: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        Ok(self
            .0
            .execute(request)
            .await?
            .with_receipt_id("sha256:test"))
    }
}

/// Local evidence fast path: the same operation requested twice. The caller
/// (this function) makes both requests explicitly.
async fn run_evidence_demo(
    title: &str,
    capability: &'static str,
    capabilities: Arc<dyn CapabilityProvider>,
    executor: Arc<dyn Executor>,
    skippable: bool,
) -> ProofOutcome {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let model_calls = Arc::new(AtomicUsize::new(0));
    let executions = Arc::new(AtomicUsize::new(0));
    let agent = Agent::new(Arc::new(ProofModel(model_calls.clone())))
        .with_capabilities(capabilities)
        .with_executor(Arc::new(CountingExecutor {
            inner: executor,
            calls: executions.clone(),
        }))
        .with_observer(Arc::new(ExecutionObserver));
    let id = |n: u32| ExecutionId::new(format!("evidence-{n}"));
    let request = |n: u32| match CapabilityId::new(capability) {
        Ok(capability_id) => Ok(CapabilityRequest::new(id(n), capability_id)),
        Err(e) => Err(e.to_string()),
    };
    let (first, second) = match (request(1), request(2)) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => return ProofOutcome::Failed(e),
    };

    if skippable {
        if let Ok(found) = agent.discover_capabilities().await.result {
            for capability in found {
                if let CapabilityAvailability::Unavailable(reason)
                | CapabilityAvailability::Misconfigured(reason) = capability.availability
                {
                    return ProofOutcome::Skipped(reason);
                }
            }
        }
    }

    println!("{title}\n");
    let observation = match agent.obtain_evidence(&first).await {
        Ok(EvidenceOutcome::Performed { observation, .. }) => observation,
        Ok(EvidenceOutcome::Reused(_)) => return ProofOutcome::Failed("unexpected reuse".into()),
        Err(AgentError::Execution(ExecutionError::ExecutorUnavailable(reason))) if skippable => {
            return ProofOutcome::Skipped(reason);
        }
        Err(e) => return ProofOutcome::Failed(e.to_string()),
    };
    println!(
        "First request:\n  Capability: {capability}\n  Execution: performed\n  Observation: {}\n  Receipt: {}\n",
        observation.kind.as_str(),
        observation.receipt_id.as_deref().unwrap_or("none")
    );

    let (models_before, runs_before) = (
        model_calls.load(Ordering::SeqCst),
        executions.load(Ordering::SeqCst),
    );
    match agent.obtain_evidence(&second).await {
        Ok(EvidenceOutcome::Reused(reused)) if reused == observation => {}
        Ok(_) => return ProofOutcome::Failed("second request did not reuse the evidence".into()),
        Err(e) => return ProofOutcome::Failed(e.to_string()),
    }
    let models_added = model_calls.load(Ordering::SeqCst) - models_before;
    if executions.load(Ordering::SeqCst) != runs_before {
        return ProofOutcome::Failed("second request executed".into());
    }
    println!(
        "Second request:\n  Capability: {capability}\n  Evidence: reused\n  Execution: skipped\n  Receipt: {}\n  Model calls added: {models_added}\n",
        observation.receipt_id.as_deref().unwrap_or("none")
    );

    let stats = agent.evidence_stats();
    println!(
        "Proof:\n  Executions: {}\n  Observations: 1\n  Evidence reuses: {}\n  Evidence lookups: {} (hits {}, misses {})\n",
        executions.load(Ordering::SeqCst),
        stats.hits,
        stats.lookups,
        stats.hits,
        stats.misses
    );
    ProofOutcome::Completed
}

/// Evidence validity demo: the same operation under an explicit state token that
/// the caller changes. Chip only compares the tokens.
async fn run_validity_demo() -> Result<(), String> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let executions = Arc::new(AtomicUsize::new(0));
    let agent = Agent::new(Arc::new(ProofModel(Arc::new(AtomicUsize::new(0)))))
        .with_capabilities(Arc::new(DemoCapabilities("compute.selftest")))
        .with_executor(Arc::new(CountingExecutor {
            inner: Arc::new(TestExecutor),
            calls: executions.clone(),
        }))
        .with_observer(Arc::new(ExecutionObserver));
    let request = |n: u32| {
        CapabilityId::new("compute.selftest")
            .map(|id| CapabilityRequest::new(ExecutionId::new(format!("validity-{n}")), id))
            .map_err(|e| e.to_string())
    };
    let step = |label: &str, outcome: &EvidenceOutcome| {
        println!(
            "{label}:\n  Execution: {}\n  Evidence: {}\n",
            if matches!(outcome, EvidenceOutcome::Performed { .. }) {
                "performed"
            } else {
                "skipped"
            },
            if matches!(outcome, EvidenceOutcome::Performed { .. }) {
                "recorded"
            } else {
                "reused"
            },
        )
    };
    let (f1, f2) = (StateToken::new("F1"), StateToken::new("F2"));

    println!("Evidence Validity\n\nState: F1\n");
    let first = agent
        .obtain_evidence_under(&request(1)?, &f1)
        .await
        .map_err(|e| e.to_string())?;
    step("First request", &first);
    let repeat = agent
        .obtain_evidence_under(&request(2)?, &f1)
        .await
        .map_err(|e| e.to_string())?;
    step("Repeat with F1", &repeat);

    println!("State changed: F2\n");
    let stale_before = agent.evidence_stats().stale;
    let performed = agent
        .obtain_evidence_under(&request(3)?, &f2)
        .await
        .map_err(|e| e.to_string())?;
    println!(
        "Request with F2:\n  Evidence: {}\n  Execution: {}\n",
        if agent.evidence_stats().stale == stale_before + 1 {
            "stale"
        } else {
            "unexpected"
        },
        if matches!(performed, EvidenceOutcome::Performed { .. }) {
            "performed"
        } else {
            "skipped"
        },
    );
    let repeat = agent
        .obtain_evidence_under(&request(4)?, &f2)
        .await
        .map_err(|e| e.to_string())?;
    step("Repeat with F2", &repeat);

    let stats = agent.evidence_stats();
    println!(
        "Proof:\n  Executions: {}\n  Evidence reuses: {}\n  Stale: {}\n",
        executions.load(Ordering::SeqCst),
        stats.hits,
        stats.stale
    );
    if executions.load(Ordering::SeqCst) == 2 && stats.hits == 2 && stats.stale == 1 {
        Ok(())
    } else {
        Err("unexpected evidence counts".into())
    }
}

/// Local reasoner proof: evidence first, local reasoning second, the model only
/// when the caller explicitly escalates. Deterministic; no live model.
async fn run_local_reasoner_demo() -> Result<(), String> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (models, executions) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    // Stale evidence is acceptable to this policy; unknown evidence is not.
    let policy = TestLocalReasoner::default().on(
        EvidenceState::KnownStale,
        LocalReasoningResult::Continue {
            rationale: "stale evidence is acceptable here".into(),
        },
    );
    let agent = Agent::new(Arc::new(ProofModel(models.clone())))
        .with_capabilities(Arc::new(DemoCapabilities("compute.selftest")))
        .with_executor(Arc::new(CountingExecutor {
            inner: Arc::new(TestExecutor),
            calls: executions.clone(),
        }))
        .with_observer(Arc::new(ExecutionObserver))
        .with_local_reasoner(Arc::new(policy));
    let request = |n: u32| {
        CapabilityId::new("compute.selftest")
            .map(|id| CapabilityRequest::new(ExecutionId::new(format!("reasoner-{n}")), id))
            .map_err(|e| e.to_string())
    };
    let (f1, f2) = (StateToken::new("F1"), StateToken::new("F2"));
    let counts = || {
        (
            models.load(Ordering::SeqCst),
            executions.load(Ordering::SeqCst),
        )
    };
    let fail = |e: AgentError| e.to_string();

    println!("Local Reasoner Proof\n");

    // Establish evidence under F1 (one explicit execution, not part of the cases).
    agent
        .obtain_evidence_under(&request(1)?, &f1)
        .await
        .map_err(fail)?;
    let (m0, e0) = counts();

    match agent
        .assess_evidence(&request(2)?, Some(&f1))
        .map_err(fail)?
    {
        Assessment::Reuse(_) => println!(
            "CASE 1\nEvidence: valid\nLocal reasoning: skipped\nFX calls: {}\nExecution: {}\n",
            counts().0 - m0,
            counts().1 - e0
        ),
        other => return Err(format!("case 1 unexpected: {other:?}")),
    }

    match agent
        .assess_evidence(&request(3)?, Some(&f2))
        .map_err(fail)?
    {
        Assessment::Continue { rationale } => println!(
            "CASE 2\nEvidence: stale\nLocal reasoning: continue ({rationale})\nFX calls: {}\nExecution: {}\n",
            counts().0 - m0,
            counts().1 - e0
        ),
        other => return Err(format!("case 2 unexpected: {other:?}")),
    }

    // A different operation has no evidence at all.
    let unknown = CapabilityId::new("compute.other")
        .map(|id| CapabilityRequest::new(ExecutionId::new("reasoner-4"), id))
        .map_err(|e| e.to_string())?;
    match agent.assess_evidence(&unknown, Some(&f1)).map_err(fail)? {
        Assessment::Escalate { reason } => {
            println!("CASE 3\nEvidence: unknown\nLocal reasoning: escalate ({reason})");
            println!("FX calls so far: {}", counts().0 - m0);
            // Escalation is the caller's explicit decision.
            agent
                .turn(Turn::new(
                    "Local reasoning was not confident; please decide",
                ))
                .await
                .map_err(fail)?;
            println!("FX escalation: explicit (1 model call made by the caller)");
            println!("Execution: {}\n", counts().1 - e0);
        }
        other => return Err(format!("case 3 unexpected: {other:?}")),
    }
    if counts() != (m0 + 1, e0) {
        return Err("unexpected model or execution counts".into());
    }
    Ok(())
}

/// Declares one deterministic capability for the decision demonstration.
struct DemoCapabilities(&'static str);

#[async_trait::async_trait]
impl CapabilityProvider for DemoCapabilities {
    async fn capabilities(&self) -> Result<Vec<CapabilityDescriptor>, CapabilityError> {
        Ok(vec![CapabilityDescriptor::new(
            CapabilityId::new(self.0)?,
            "Test Operation",
            "Deterministic test capability",
        )])
    }

    async fn availability(&self, _id: &CapabilityId) -> CapabilityAvailability {
        CapabilityAvailability::Available
    }
}

/// Builds provider config from CHIP_PROVIDER / CHIP_MODEL / CHIP_ENDPOINT / CHIP_API_KEY.
fn config_from_env(get: impl Fn(&str) -> Option<String>) -> Result<HttpProviderConfig, FxError> {
    let required = |name: &str| {
        get(name)
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| FxError::Configuration(format!("{name} is not set")))
    };
    let provider = get("CHIP_PROVIDER").unwrap_or_else(|| PROVIDER_OPENAI_COMPATIBLE.to_string());
    let mut config = HttpProviderConfig::new(
        provider,
        required("CHIP_MODEL")?,
        required("CHIP_ENDPOINT")?,
    );
    if let Some(key) = get("CHIP_API_KEY").filter(|k| !k.is_empty()) {
        config = config.with_api_key(Secret::new(key));
    }
    Ok(config)
}

async fn run_configured(prompt: String) -> Result<(), String> {
    let config = config_from_env(|name| std::env::var(name).ok()).map_err(|e| e.to_string())?;
    let provider_name = config.provider.clone();
    let model = config.model.to_string();
    let provider = HttpProvider::new(config).map_err(|e| e.to_string())?;
    let agent = Agent::with_model(Arc::new(provider), model);
    let result = agent
        .turn(Turn::new(prompt))
        .await
        .map_err(|e| e.to_string())?;

    println!("Chip");
    println!("FX provider: {provider_name}");
    println!("Turn completed");
    println!("{}", result.response);
    Ok(())
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() > 1 && args[1] == "init" {
        std::process::exit(graph_cmd::init(&args[2..]));
    }

    if args.len() > 1 && args[1] == "decision-state" {
        std::process::exit(decision_state_cmd::decision_state(&args[2..]));
    }

    if args.len() > 1 && args[1] == "--benchmark-decision-state" {
        std::process::exit(decision_state_cmd::benchmark(&args[2..]));
    }

    if args.len() > 1 && args[1] == "--benchmark-local-model" {
        std::process::exit(local_model_bench::benchmark(&args[2..]));
    }

    if args.len() > 1 && args[1] == "--test-work" {
        std::process::exit(work_demo::test_work(&args[2..]).await);
    }

    if args.len() > 1 && args[1] == "--benchmark-work" {
        std::process::exit(work_demo::benchmark_work(&args[2..]).await);
    }

    if args.len() > 1 && args[1] == "--test-real-work" {
        std::process::exit(work_demo::test_real_work(&args[2..]).await);
    }

    if args.len() > 1 && args[1] == "--report-decision-corpus" {
        std::process::exit(local_model_bench::report_decision_corpus(&args[2..]));
    }

    if args.len() > 1 && args[1] == "--evaluate-local-model" {
        std::process::exit(local_model_bench::evaluate_corpus(&args[2..]));
    }

    if args.len() > 1 && args[1] == "--benchmark-wasm-decision" {
        std::process::exit(wasm_decision_bench::benchmark(&args[2..]));
    }

    if args.len() > 1 && args[1] == "slice" {
        std::process::exit(graph_cmd::slice(&args[2..]));
    }

    if args.len() > 1 && args[1] == "impact" {
        std::process::exit(graph_cmd::impact(&args[2..]));
    }

    if args.len() > 1 && args[1] == "graph" {
        std::process::exit(graph_cmd::graph(&args[2..]));
    }

    if args.len() > 1 && args[1] == "--test" {
        let agent = Agent::new(Arc::new(TestModelProvider));
        let result = agent
            .turn(Turn::new("Hello"))
            .await
            .expect("turn should succeed");

        println!("Chip");
        println!("FX provider: test");
        println!("Turn completed");
        println!("{}", result.response);
        return;
    }

    if args.len() > 1 && args[1] == "--test-execution" {
        let agent = Agent::new(Arc::new(TestModelProvider)).with_executor(Arc::new(TestExecutor));
        let request = ExecutionRequest::new(ExecutionId::new("exec-1"), "test operation");
        let result = agent
            .turn_and_execute(Turn::new("Hello"), request)
            .await
            .expect("turn should succeed");

        println!("Chip");
        println!("FX provider: test");
        println!("Turn completed");
        println!("{}", result.turn.response);
        match result.execution.result {
            Ok(execution) => println!("Execution {:?}: {}", execution.status, execution.output),
            Err(error) => {
                eprintln!("error: {error}");
                std::process::exit(1);
            }
        }
        return;
    }

    if args.len() > 1 && args[1] == "--test-decision" {
        // Deterministic demonstration only: scripted decision, test executor.
        let agent = Agent::new(Arc::new(TestModelProvider))
            .with_decision_boundary(Arc::new(ScriptedDecision::new(
                DecisionInput::RequestCapability {
                    execution_id: ExecutionId::new("decision-1"),
                    capability_id: "test.operation".into(),
                    inputs: Default::default(),
                },
            )))
            .with_capabilities(Arc::new(DemoCapabilities("test.operation")))
            .with_executor(Arc::new(TestExecutor));
        let report = agent
            .decide(Turn::new("Hello"))
            .await
            .expect("turn should succeed");
        println!("Chip");
        println!("Model response: {}", report.turn.response);
        match report.decision {
            Ok(AgentDecision::RequestCapability(request)) => {
                println!("Decision: request capability {}", request.capability_id);
                match agent.execute_capability(&request).await {
                    Ok(execution) => match execution.result {
                        Ok(result) => println!("Execution {:?}: {}", result.status, result.output),
                        Err(error) => {
                            eprintln!("error: {error}");
                            std::process::exit(1);
                        }
                    },
                    Err(error) => {
                        eprintln!("error: {error}");
                        std::process::exit(1);
                    }
                }
            }
            Ok(AgentDecision::Respond(_)) => println!("Decision: respond"),
            Err(error) => {
                eprintln!("error: {error}");
                std::process::exit(1);
            }
        }
        return;
    }

    if args.len() > 1 && args[1] == "--test-turn" {
        // Deterministic lifecycle demonstration: no network, keys, or Compute.
        let agent = Agent::new(Arc::new(TestModelProvider))
            .with_decision_boundary(Arc::new(ScriptedDecision::new(
                DecisionInput::RequestCapability {
                    execution_id: ExecutionId::new("turn-1"),
                    capability_id: "test.operation".into(),
                    inputs: Default::default(),
                },
            )))
            .with_capabilities(Arc::new(DemoCapabilities("test.operation")))
            .with_executor(Arc::new(TestExecutor));
        match agent.run_turn(Turn::new("Hello")).await {
            Ok(outcome) => {
                println!("Chip");
                println!("Model response: {}", outcome.response.output);
                match &outcome.decision {
                    AgentDecision::RequestCapability(request) => {
                        println!("Decision: request capability {}", request.capability_id)
                    }
                    AgentDecision::Respond(_) => println!("Decision: respond"),
                }
                match &outcome.execution {
                    Some(result) => println!("Execution {:?}: {}", result.status, result.output),
                    None => println!("Execution: none"),
                }
                println!("Events: {}", outcome.events.len());
                println!("Turn completed");
            }
            Err(error) => {
                eprintln!("error: {error}");
                std::process::exit(1);
            }
        }
        return;
    }

    if args.len() > 1 && args[1] == "--test-observation" {
        // Deterministic and offline: a fixed result passed through the observer.
        let agent =
            Agent::new(Arc::new(TestModelProvider)).with_observer(Arc::new(ExecutionObserver));
        let result = ExecutionResult::success(ExecutionId::new("observation-1"), "hello")
            .with_receipt_id("sha256:test-receipt");
        match agent.observe(&result) {
            Ok(observation) => {
                println!("Chip");
                println!("Execution: success");
                println!("Observation: {}", observation.kind.as_str());
                println!("Execution ID: {}", observation.execution_id);
                println!("Output: {}", observation.output.as_deref().unwrap_or(""));
                println!(
                    "Receipt: {}",
                    observation.receipt_id.as_deref().unwrap_or("none")
                );
            }
            Err(error) => {
                eprintln!("error: {error}");
                std::process::exit(1);
            }
        }
        return;
    }

    if args.len() > 1 && args[1] == "--test-cycle" {
        // Caller-driven bounded cycle; every step below is an explicit call.
        let agent = Agent::new(Arc::new(CycleModel))
            .with_decision_boundary(Arc::new(CycleDecisions(
                Default::default(),
                "test.operation",
            )))
            .with_capabilities(Arc::new(DemoCapabilities("test.operation")))
            .with_executor(Arc::new(TestExecutor))
            .with_observer(Arc::new(ExecutionObserver));
        println!("Chip bounded cycle (caller-driven, not autonomous)");
        let outcome: Result<(), String> = async {
            let first = agent
                .decide(Turn::new("Run the test operation"))
                .await
                .map_err(|e| e.to_string())?;
            let Ok(AgentDecision::RequestCapability(request)) = first.decision else {
                return Err("turn 1 did not request a capability".into());
            };
            println!("Turn 1: capability requested ({})", request.capability_id);
            let report = agent
                .execute_capability(&request)
                .await
                .map_err(|e| e.to_string())?;
            let result = report.result.map_err(|e| e.to_string())?;
            println!(
                "Execution: {}",
                format!("{:?}", result.status).to_lowercase()
            );
            let observation = agent.observe(&result).map_err(|e| e.to_string())?;
            println!("Observation: {}", observation.kind.as_str());
            let second = agent
                .decide_with_observations(
                    Turn::new("What happened?"),
                    std::slice::from_ref(&observation),
                )
                .await
                .map_err(|e| e.to_string())?;
            match second.decision {
                Ok(AgentDecision::Respond(response)) => {
                    println!("Turn 2: response ({})", response.output)
                }
                Ok(AgentDecision::RequestCapability(_)) => {
                    println!("Turn 2: capability requested (not executed)")
                }
                Err(e) => return Err(e.to_string()),
            }
            Ok(())
        }
        .await;
        match outcome {
            Ok(()) => println!("Cycle completed"),
            Err(error) => {
                eprintln!("error: {error}");
                std::process::exit(1);
            }
        }
        return;
    }

    if args.len() > 1 && args[1] == "--test-workload" {
        // Deterministic: the test executor stands in for Compute.
        let outcome = run_workload_proof(
            "Bounded Workload Proof (deterministic, test executor)",
            "compute.selftest",
            Arc::new(DemoCapabilities("compute.selftest")),
            Arc::new(TestExecutor),
            false,
        )
        .await;
        match outcome {
            ProofOutcome::Completed => println!("Bounded workload completed."),
            ProofOutcome::Skipped(reason) | ProofOutcome::Failed(reason) => {
                eprintln!("error: {reason}");
                std::process::exit(1);
            }
        }
        return;
    }

    if args.len() > 1 && args[1] == "--test-evidence" {
        let outcome = run_evidence_demo(
            "Local Evidence Fast Path",
            "compute.selftest",
            Arc::new(DemoCapabilities("compute.selftest")),
            Arc::new(FixedReceipt(Arc::new(TestExecutor))),
            false,
        )
        .await;
        match outcome {
            ProofOutcome::Completed => println!("Local fast path completed."),
            ProofOutcome::Skipped(reason) | ProofOutcome::Failed(reason) => {
                eprintln!("error: {reason}");
                std::process::exit(1);
            }
        }
        return;
    }

    if args.len() > 1 && args[1] == "--test-evidence-validity" {
        match run_validity_demo().await {
            Ok(()) => println!("Evidence validity completed."),
            Err(error) => {
                eprintln!("error: {error}");
                std::process::exit(1);
            }
        }
        return;
    }

    if args.len() > 1 && args[1] == "--test-local-reasoner" {
        match run_local_reasoner_demo().await {
            Ok(()) => println!("Local reasoner proof completed."),
            Err(error) => {
                eprintln!("error: {error}");
                std::process::exit(1);
            }
        }
        return;
    }

    if args.len() > 1 && args[1] == "--test-reasoning-corpus" {
        // Offline replay. Mismatches against the corpus are reported, not fatal;
        // a Rust/WASM disagreement or a WASM failure is.
        match corpus_eval::run() {
            Ok(report) => {
                print!("{}", corpus_eval::render(&report));
                if !report.disagreements.is_empty()
                    || report.wasm.cases.iter().any(|c| c.error.is_some())
                {
                    std::process::exit(1);
                }
            }
            Err(error) => {
                eprintln!("error: {error}");
                std::process::exit(1);
            }
        }
        return;
    }

    if args.len() > 1 && args[1] == "--test-laya-reasoner" {
        // Experimental. Needs an explicit local checkpoint directory (argument or
        // CHIP_LAYA_MODEL_DIR); nothing is downloaded. Exit 3 means SKIPPED.
        let location = native::laya_location(args.get(2).map(String::as_str));
        match laya_eval::run(location) {
            Ok((report, laya)) => print!("{}", laya_eval::render(&report, &laya)),
            Err(reason) => {
                println!("Laya reasoner: SKIPPED\nReason: {reason}");
                std::process::exit(3);
            }
        }
        return;
    }

    if args.len() > 1 && args[1] == "--test-local-model-reasoner" {
        // Optional infrastructure: exit 3 (SKIPPED) when the local model is not available.
        match native::load() {
            Err(reason) => {
                println!("SKIPPED — local model unavailable ({reason})");
                std::process::exit(3);
            }
            Ok(model) => {
                #[cfg(feature = "local-ml")]
                match native::demo(&model) {
                    Ok(text) => print!("{text}"),
                    Err(error) => {
                        eprintln!("error: {error}");
                        std::process::exit(1);
                    }
                }
                #[cfg(not(feature = "local-ml"))]
                let _ = model;
            }
        }
        return;
    }

    if args.len() > 1 && args[1] == "--benchmark-local-reasoner" {
        // Informational timings; correctness and call counts are asserted inside.
        let per_state = args.get(2).and_then(|n| n.parse().ok()).unwrap_or(10_000);
        match benchmark::run(per_state).await {
            Ok(report) => print!("{}", benchmark::render(&report)),
            Err(error) => {
                eprintln!("error: {error}");
                std::process::exit(1);
            }
        }
        return;
    }

    if args.len() > 1 && args[1] == "--benchmark-live-reasoner" {
        // Optional: needs the same CHIP_* configuration as a live model request.
        // Exit status 3 means SKIPPED. Never part of the deterministic acceptance path.
        let config = match config_from_env(|name| std::env::var(name).ok()) {
            Ok(config) => config,
            Err(_) => {
                println!(
                    "SKIPPED — live provider not configured (set CHIP_MODEL and CHIP_ENDPOINT)"
                );
                std::process::exit(3);
            }
        };
        let count: usize = args.get(2).and_then(|n| n.parse().ok()).unwrap_or(5);
        let meta = live_benchmark::Meta::new(
            &config.provider,
            &config.model.to_string(),
            &config.endpoint,
        );
        let secret = config.api_key.as_ref().map(|k| k.expose().to_string());
        let provider = match HttpProvider::new(config) {
            Ok(provider) => provider,
            Err(error) => {
                eprintln!("error: {error}");
                std::process::exit(1);
            }
        };
        let report = live_benchmark::run(Arc::new(provider), meta, count).await;
        // Local reference, measured now so nothing is hard-coded.
        let local = benchmark::run(200)
            .await
            .ok()
            .map(|r| live_benchmark::LocalReference {
                evidence_hit: r.evidence_hit.stats.median,
                rust: r.rust.stats.median,
                wasm: r.wasm.stats.median,
            });
        print!(
            "{}",
            live_benchmark::render(&report, local.as_ref(), secret.as_deref())
        );
        if report.errors() > 0 {
            std::process::exit(1);
        }
        return;
    }

    if args.len() > 1 && args[1] == "--test-real-evidence" {
        // Real Compute once; the repeat must not invoke it. Exit 3 means SKIPPED.
        let compute = Arc::new(chip_compute::ComputeExecutor::new());
        let outcome = run_evidence_demo(
            "Local Evidence Fast Path (real Compute)",
            chip_compute::SELFTEST_INTENT,
            compute.clone(),
            compute,
            true,
        )
        .await;
        match outcome {
            ProofOutcome::Completed => println!("Local fast path completed."),
            ProofOutcome::Skipped(reason) => {
                println!("SKIPPED — Compute unavailable ({reason})");
                std::process::exit(3);
            }
            ProofOutcome::Failed(reason) => {
                eprintln!("error: {reason}");
                std::process::exit(1);
            }
        }
        return;
    }

    if args.len() > 1 && args[1] == "--test-real-cycle" {
        // Live: real Compute via COMPUTE_BIN (default `compute`). Never falls
        // back to the test executor; exit status 3 means SKIPPED.
        let compute = Arc::new(chip_compute::ComputeExecutor::new());
        let outcome = run_workload_proof(
            "Bounded Workload Proof (real Compute)",
            chip_compute::SELFTEST_INTENT,
            compute.clone(),
            compute,
            true,
        )
        .await;
        match outcome {
            ProofOutcome::Completed => println!("Bounded workload completed."),
            ProofOutcome::Skipped(reason) => {
                println!("SKIPPED — Compute unavailable ({reason})");
                std::process::exit(3);
            }
            ProofOutcome::Failed(reason) => {
                eprintln!("error: {reason}");
                std::process::exit(1);
            }
        }
        return;
    }

    if args.len() > 1 && args[1] == "--compute-test" {
        // Explicit, opt-in: runs one real Compute execution. Never a fallback.
        let agent = Agent::new(Arc::new(TestModelProvider))
            .with_executor(Arc::new(chip_compute::ComputeExecutor::new()));
        let request = ExecutionRequest::new(
            ExecutionId::new("compute-test-1"),
            chip_compute::SELFTEST_INTENT,
        );
        match agent.execute(request).await.result {
            Ok(result) => {
                println!("Chip");
                println!("Compute execution {:?}: {}", result.status, result.output);
                if let Some(receipt) = result.receipt_id {
                    println!("Receipt: {receipt}");
                }
                if result.status != chip_core::ExecutionStatus::Success {
                    std::process::exit(1);
                }
            }
            Err(error) => {
                eprintln!("error: {error}");
                std::process::exit(1);
            }
        }
        return;
    }

    if args.len() > 1 {
        if let Err(message) = run_configured(args[1..].join(" ")).await {
            eprintln!("error: {message}");
            std::process::exit(1);
        }
        return;
    }

    eprintln!("Usage: cargo run -p chip-cli -- --test");
    eprintln!(
        "       CHIP_MODEL=.. CHIP_ENDPOINT=.. [CHIP_PROVIDER=..] [CHIP_API_KEY=..] cargo run -p chip-cli -- \"<prompt>\""
    );
}

#[cfg(test)]
mod tests {
    use super::config_from_env;
    use fx_core::FxError;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn reads_all_fields_and_redacts_key() {
        let config = config_from_env(env(&[
            ("CHIP_PROVIDER", "openai-compatible"),
            ("CHIP_MODEL", "m"),
            ("CHIP_ENDPOINT", "http://localhost/x"),
            ("CHIP_API_KEY", "super-secret-value"),
        ]))
        .unwrap();
        assert_eq!(config.model.0, "m");
        assert_eq!(config.endpoint, "http://localhost/x");
        assert!(config.api_key.is_some());
        assert!(!format!("{config:?}").contains("super-secret-value"));
    }

    #[test]
    fn missing_model_or_endpoint_is_configuration_error() {
        let err = config_from_env(env(&[("CHIP_ENDPOINT", "http://x")])).unwrap_err();
        assert!(matches!(err, FxError::Configuration(_)));
        let err = config_from_env(env(&[("CHIP_MODEL", "m")])).unwrap_err();
        assert!(matches!(err, FxError::Configuration(_)));
    }
}
