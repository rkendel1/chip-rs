//! `chip --test-pax-work --workdir <directory>`: a real model asks for `pax.test`; Chip
//! validates the request; the installed PAX is run against the work directory; the real process
//! observation is recorded.
//!
//! PAX's `pax.execution-result.v1` is the authority: the goal is "PAX established the test
//! operation as passed", decided from the validated result's `status` only (never an exit code,
//! never text, never a model's claim). Chip does not learn any ecosystem's output.
//!
//! The CLI exits 0 when the chain happened and the safety audit is clean, whatever the project's
//! tests did (the printed terminal state and PAX status say what happened); 3 when no provider or
//! no usable PAX is available; 1 when the audit finds a violation or the model never made a valid
//! request for the capability.

use std::sync::{Arc, Mutex};

use chip_core::{
    Agent, CapabilityAvailability, CapabilityId, CapabilityProvider, ExecutionObserver,
    ModelDecisionBoundary, TestLocalReasoner, WorkEvent, WorkGoal, WorkId, WorkLimits, WorkOutcome,
    WorkSpec, audit_safety, measure_utility, verify_trajectory,
};
use chip_pax::{PAX_TEST_CAPABILITY, PaxExecutor, PaxTestPassed, parse_execution_result};

use crate::verify::{ReactToObservation, Recording};

pub async fn test_pax_work(args: &[String]) -> i32 {
    let workdir = match args
        .iter()
        .position(|a| a == "--workdir")
        .and_then(|i| args.get(i + 1))
    {
        Some(dir) => dir.clone(),
        None => {
            eprintln!("error: --workdir <directory> is required");
            return 2;
        }
    };
    let print_reply = args.iter().any(|a| a == "--print-reply");

    let config = match crate::config_from_env(|name| std::env::var(name).ok()) {
        Ok(config) => config,
        Err(e) => {
            println!("SKIPPED: real model provider unavailable ({e})");
            return 3;
        }
    };
    let (provider_name, model_name) = (config.provider.clone(), config.model.to_string());
    let provider = match fx_provider_http::HttpProvider::new(config) {
        Ok(provider) => provider,
        Err(e) => {
            println!("SKIPPED: real model provider unavailable ({e})");
            return 3;
        }
    };

    let pax = PaxExecutor::new(&workdir);
    let resolved = match pax.resolve().await {
        Ok(resolved) => resolved,
        Err(why) => {
            // Fail closed: nothing is run and nothing is observed.
            println!("SKIPPED: PAX unavailable ({why})");
            return 3;
        }
    };

    let replies = Arc::new(Mutex::new(Vec::new()));
    let provider = Recording {
        inner: provider,
        replies: replies.clone(),
    };
    let agent = Agent::with_model(Arc::new(provider), model_name.clone())
        .with_capabilities(Arc::new(pax.clone()))
        .with_executor(Arc::new(pax.clone()))
        .with_observer(Arc::new(ExecutionObserver))
        .with_local_reasoner(Arc::new(TestLocalReasoner::default()));
    let limits = WorkLimits {
        max_turns: 3,
        max_executions: 1,
    };
    let spec = WorkSpec::new(
        WorkId::new("pax-work"),
        WorkGoal::new("Verify that the project's tests pass."),
    )
    .with_limits(limits)
    .with_required_observation(Arc::new(PaxTestPassed));
    let report = agent
        .run_work(&spec, &ReactToObservation, &ModelDecisionBoundary)
        .await;

    let declared = vec![CapabilityId::new(PAX_TEST_CAPABILITY).unwrap()];
    let audit = audit_safety(&report, &spec, &declared);
    let violations = verify_trajectory(&report.events, &limits);
    let m = report.measurement();
    let u = measure_utility(&report, &spec);
    let offered = matches!(
        pax.availability(&declared[0]).await,
        CapabilityAvailability::Available
    );
    let observation = report.observations.last();
    // What PAX said: line one of the recorded observation, parsed by the same strict parser.
    let said = observation
        .and_then(|o| o.output.as_deref())
        .and_then(|t| t.lines().next())
        .and_then(|l| parse_execution_result(l.as_bytes()).ok());
    let goal_satisfied = report.events.iter().rev().find_map(|e| match e {
        WorkEvent::GoalEvaluated { satisfied, .. } => Some(*satisfied),
        _ => None,
    });
    let invocation: Vec<String> = std::iter::once(resolved.path.display().to_string())
        .chain(
            pax.invocation()
                .iter()
                .map(|a| a.to_string_lossy().into_owned()),
        )
        .collect();

    println!("PAX capability work");
    println!("-------------------");
    println!("Provider:            {provider_name}");
    println!("Model:               {model_name}");
    println!("Work directory:      {workdir}");
    println!(
        "PAX:                 {} (version {})",
        resolved.path.display(),
        resolved.version
    );
    println!("Invocation:          {}", invocation.join(" "));
    println!(
        "Capability offered:  {} ({})",
        PAX_TEST_CAPABILITY,
        if offered { "available" } else { "unavailable" }
    );
    println!(
        "Terminal:            {}",
        match &report.outcome {
            WorkOutcome::Completed { .. } => "Completed".to_string(),
            WorkOutcome::Escalated { reason } => format!("Escalated ({reason})"),
            WorkOutcome::Blocked { reason } => format!("Blocked ({reason})"),
            WorkOutcome::Failed { reason } => format!("Failed ({reason})"),
            WorkOutcome::LimitReached { limit } => format!("LimitReached ({})", limit.name()),
        }
    );
    println!("Turns:               {}", m.turns);
    println!("Model calls:         {}", m.model_calls);
    println!("Executions:          {}", u.executions);
    println!("Observations:        {}", m.observations);
    println!(
        "Evidence records:    {}",
        report
            .events
            .iter()
            .filter(|e| matches!(e, WorkEvent::EvidenceRecorded { .. }))
            .count()
    );
    println!("Receipt:             none (PAX issues no receipt)");
    match &said {
        Some(r) => {
            println!("PAX status:          {}", r.status.as_str());
            println!("PAX reason:          {}", r.reason);
            println!(
                "PAX tool:            {}",
                r.tool.as_deref().unwrap_or("none")
            );
            println!(
                "Native exit code:    {}",
                r.exit_code.map_or("none".to_string(), |c| c.to_string())
            );
            match r.tests {
                Some(t) => println!(
                    "Tests:               passed={} failed={} ignored={} measured={}",
                    t.passed, t.failed, t.ignored, t.measured
                ),
                None => println!("Tests:               not reported"),
            }
        }
        None => println!("PAX status:          none (no valid result was observed)"),
    }
    println!(
        "Goal satisfied:      {}",
        goal_satisfied.map_or("not evaluated".to_string(), |s| s.to_string())
    );
    println!(
        "Verified outputs:    {} of {} (goal coverage {:.2})",
        u.verified_outputs, u.required_outputs, u.goal_coverage
    );
    println!(
        "Tokens:              {}",
        u.total_tokens
            .map_or("not reported".to_string(), |t| t.to_string())
    );
    println!(
        "Model latency:       {:.0} ms",
        m.model_latency.as_secs_f64() * 1000.0
    );
    println!(
        "PAX latency:         {:.0} ms",
        m.compute_latency.as_secs_f64() * 1000.0
    );
    println!(
        "Total latency:       {:.0} ms",
        m.total_latency.as_secs_f64() * 1000.0
    );
    println!(
        "Audit:               unauthorized_executions={} unauthorized_completions={} false_completions={} evidence_without_observation={} observation_without_execution={} execution_without_valid_request={} limit_violations={}",
        audit.unauthorized_executions,
        audit.unauthorized_completions,
        audit.false_completions,
        audit.evidence_without_observation,
        audit.observation_without_execution,
        audit.execution_without_valid_request,
        audit.limit_violations
    );
    println!("\nTrajectory:");
    for (i, shape) in crate::horizon::shape(&report).iter().enumerate() {
        println!("  {:>2}. {shape}", i + 1);
    }
    if let Some(text) = observation.and_then(|o| o.output.as_deref()) {
        println!("\nObservation (as recorded):");
        for line in text.lines().take(40) {
            println!("  | {line}");
        }
        if text.lines().count() > 40 {
            println!("  | ... ({} more lines)", text.lines().count() - 40);
        }
    }
    if print_reply {
        for reply in replies.lock().unwrap().iter() {
            println!("\nModel reply:\n{reply}");
        }
    }

    if !audit.is_clean() || !violations.is_empty() {
        eprintln!("\nSAFETY INVARIANT VIOLATED: {audit:#?} {violations:?}");
        return 1;
    }
    if u.executions == 1 && m.observations == 1 {
        0
    } else {
        eprintln!(
            "\nerror: the chain did not occur: the model made no valid request for {PAX_TEST_CAPABILITY} (outcome: {:?})",
            report.outcome
        );
        1
    }
}
