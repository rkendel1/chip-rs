//! Section `fxlive`: the OPTIONAL real-endpoint measurement.
//!
//! One OpenAI-compatible endpoint that *you* name (LM Studio, Ollama's `/v1`, llama.cpp, a hosted
//! API), through the same product path as `chip work`: `build_model` with the FX executor and the
//! native provider, then the real work loop. It exists to put a number next to the deterministic
//! `fx` section so the two can be compared, never to be mixed with it:
//!
//! * it never runs unless `--only fxlive` is given (a default run makes no network call at all);
//! * there is no default endpoint and no default model;
//! * no credential is needed for a keyless local server; a key, if given, is read from
//!   `CHIP_PERF_API_KEY`, handed to the provider and never written to the report or the console;
//! * a non-local host additionally needs `--allow-remote` (it may be paid and is non-deterministic);
//! * a fixed one-line prompt, bounded output, `--iterations` requests (default 3, at most 50), no
//!   warm-up, no retry: a failed iteration is recorded as an error and the next one still runs;
//! * its results land in the report's `real_model` object and in `L2` / `class: real-model` rows.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chip_cli::provider_selection::{
    CA_BUNDLE_VARIABLE, ExecutorKind, build_model, endpoint_identity,
};
use chip_core::{
    Agent, ExecutionObserver, ModelDecisionBoundary, NoLocalPolicy, WorkGoal, WorkId, WorkLimits,
    WorkSpec,
};
use chip_fx_native::LAN_HTTP_VARIABLE;
use chip_inference::{
    FxError, ModelProvider, ModelRequest, ModelResponse, PROVIDER_OPENAI_COMPATIBLE,
    ProviderConfig, Secret,
};

use super::{Noop, Opts, Report};

/// The fixed goal. It names no project, path or secret; its id goes in the report, not its text.
const PROMPT_ID: &str = "fxlive-prompt-v1";
const GOAL: &str = "Reply with the completion decision now. Do not request any capability.";
/// Output bound for every request. Large enough for a model that reasons before it answers: a
/// 1,186-token completion was measured for this prompt, almost all of it reasoning, and a lower bound
/// cut the reply off before the decision. The bound is recorded in the report, so a change is visible.
const MAX_TOKENS: u32 = 4096;
const TIMEOUT: Duration = Duration::from_secs(120);
pub const DEFAULT_ITERATIONS: usize = 3;
pub const MAX_ITERATIONS: usize = 50;

pub const LABEL: &str = "OPTIONAL and NON-DETERMINISTIC: a real model behind a real network path. \
Not comparable with the deterministic `fx` scenarios, and not a statement about any other model, \
machine or time.";

#[derive(Debug)]
pub struct LiveConfig {
    pub endpoint: String,
    pub model: String,
    pub iterations: usize,
    pub endpoint_type: &'static str,
    pub api_key: Option<String>,
}

/// Where the endpoint is, from its host alone (no DNS lookup, no request).
fn endpoint_type(host: &str) -> &'static str {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let host = host.rsplit_once(':').map_or(host, |(h, p)| {
        if p.chars().all(|c| c.is_ascii_digit()) && !h.contains(':') {
            h
        } else {
            host
        }
    });
    let octets: Vec<u8> = host.split('.').filter_map(|p| p.parse().ok()).collect();
    if host == "localhost" || host == "::1" || octets.first() == Some(&127) {
        "loopback"
    } else if octets.len() == 4
        && (octets[0] == 10
            || (octets[0] == 192 && octets[1] == 168)
            || (octets[0] == 172 && (16..32).contains(&octets[1])))
    {
        "private-lan"
    } else {
        "remote"
    }
}

impl LiveConfig {
    /// Validates everything before any request is made. The error says what to change.
    pub fn resolve(
        opts: &Opts,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<LiveConfig, String> {
        let nonempty = |v: Option<String>| v.filter(|s| !s.trim().is_empty());
        let endpoint = nonempty(opts.endpoint.clone().or_else(|| env("CHIP_PERF_ENDPOINT")))
            .ok_or("fxlive needs an endpoint: --endpoint URL or CHIP_PERF_ENDPOINT (for example http://127.0.0.1:1234/v1/chat/completions). There is no default, so no request was made")?;
        let model = nonempty(opts.model.clone().or_else(|| env("CHIP_PERF_MODEL")))
            .ok_or("fxlive needs a model id: --model ID or CHIP_PERF_MODEL. There is no default, so no request was made")?;
        let iterations = match &opts.iterations {
            None => DEFAULT_ITERATIONS,
            Some(raw) => raw
                .parse::<usize>()
                .ok()
                .filter(|n| (1..=MAX_ITERATIONS).contains(n))
                .ok_or_else(|| {
                    format!("--iterations must be a whole number from 1 to {MAX_ITERATIONS}, got {raw:?}")
                })?,
        };
        let (scheme, rest) = endpoint.split_once("://").ok_or_else(|| {
            "the endpoint must be a URL such as http://127.0.0.1:1234/v1/chat/completions"
                .to_string()
        })?;
        if scheme != "http" && scheme != "https" {
            return Err(format!(
                "the endpoint scheme must be http or https, got {scheme:?}"
            ));
        }
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        if authority.contains('@') {
            return Err("the endpoint must not contain credentials; use CHIP_PERF_API_KEY".into());
        }
        if authority.is_empty() {
            return Err("the endpoint has no host".into());
        }
        let kind = endpoint_type(authority);
        if kind == "remote" && !opts.allow_remote {
            return Err(format!(
                "{} is not a loopback or private-LAN host. A remote endpoint may be paid and is \
                 non-deterministic; add --allow-remote to run it anyway. No request was made",
                endpoint_identity(&endpoint)
            ));
        }
        Ok(LiveConfig {
            endpoint,
            model,
            iterations,
            endpoint_type: kind,
            api_key: nonempty(env("CHIP_PERF_API_KEY")),
        })
    }
}

/// One recorded call.
struct Call {
    elapsed: Duration,
    /// (prompt, completion) tokens as the endpoint reported them.
    tokens: Option<(u32, u32)>,
    error: Option<String>,
}

/// Times each `complete()` of the real provider and keeps its usage. Harness only.
struct Timed {
    inner: Arc<dyn ModelProvider>,
    calls: Mutex<Vec<Call>>,
}

#[async_trait::async_trait]
impl ModelProvider for Timed {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, FxError> {
        let t = Instant::now();
        let out = self.inner.complete(request).await;
        let elapsed = t.elapsed();
        let (usage, error) = match &out {
            Ok(r) => (
                Some((r.usage.prompt_tokens, r.usage.completion_tokens)),
                None,
            ),
            Err(e) => (None, Some(e.to_string())),
        };
        self.calls.lock().unwrap().push(Call {
            elapsed,
            tokens: usage,
            error,
        });
        out
    }
}

/// An error text safe to keep: the key (if it ever appears) removed and the length bounded.
fn scrub(text: &str, key: Option<&str>) -> String {
    let mut text = text.to_string();
    if let Some(key) = key.filter(|k| !k.is_empty()) {
        text = text.replace(key, "[redacted]");
    }
    text.chars().take(240).collect()
}

/// The variant name of a `WorkOutcome`, never its text (a reason or summary may quote the model).
fn outcome_kind(outcome: &chip_core::WorkOutcome) -> String {
    let debug = format!("{outcome:?}");
    debug
        .split(|c: char| !c.is_alphanumeric())
        .next()
        .unwrap_or("Unknown")
        .to_string()
}

pub async fn fxlive(report: &mut Report, cfg: &LiveConfig) {
    let mut config = ProviderConfig::new(PROVIDER_OPENAI_COMPATIBLE, &cfg.model, &cfg.endpoint)
        .with_timeout(TIMEOUT);
    config.max_tokens = Some(MAX_TOKENS);
    config.temperature = Some(0.0);
    if let Some(key) = &cfg.api_key {
        config = config.with_api_key(Secret::new(key.clone()));
    }
    // Only the LAN opt-in and the CA bundle (public certificates, not a credential) come from the
    // process environment; no provider key variable is read, so a stray ANTHROPIC_API_KEY or
    // OPENAI_API_KEY can never be sent to the endpoint named here.
    let env = |name: &str| {
        (name == LAN_HTTP_VARIABLE || name == CA_BUNDLE_VARIABLE)
            .then(|| std::env::var(name).ok())
            .flatten()
    };
    let model = match build_model(config, ExecutorKind::Fx, &env) {
        Ok(m) => m,
        Err(e) => {
            let why = scrub(&e.to_string(), cfg.api_key.as_deref());
            report.error("fxlive.configuration", &why);
            report.real_model = serde_json::json!({
                "label": LABEL, "status": "not-run", "error": why, "requests_made": 0,
            });
            return;
        }
    };

    let (mut inference, mut turns, mut overhead) = (Vec::new(), Vec::new(), Vec::new());
    let mut iterations = Vec::new();
    let mut requests = 0usize;
    for i in 0..cfg.iterations {
        let timed = Arc::new(Timed {
            inner: model.clone(),
            calls: Mutex::new(Vec::new()),
        });
        let caps = Arc::new(Noop {
            executed: Default::default(),
        });
        // The agent builds each request with its own output bound and ignores
        // `ProviderConfig.max_tokens`, so the bound is set here as well; otherwise the report would
        // record one value and the request carry another (the agent's small default).
        let agent = Agent::with_model(timed.clone(), cfg.model.clone())
            .with_max_output_tokens(MAX_TOKENS)
            .with_capabilities(caps.clone())
            .with_executor(caps)
            .with_observer(Arc::new(ExecutionObserver));
        // One turn, no execution: at most one request per iteration, whatever the model answers.
        let spec = WorkSpec::new(WorkId::new(format!("fxlive-{i}")), WorkGoal::new(GOAL))
            .with_limits(WorkLimits {
                max_turns: 1,
                max_executions: 0,
            });
        let work = agent
            .run_work(&spec, &NoLocalPolicy, &ModelDecisionBoundary)
            .await;
        let calls = timed.calls.lock().unwrap();
        requests += calls.len();
        let call = calls.first();
        let outcome = outcome_kind(&work.outcome);
        let error = call
            .and_then(|c| c.error.as_deref())
            .map(|e| scrub(e, cfg.api_key.as_deref()));
        if let Some(e) = &error {
            report.error(&format!("fxlive.iteration.{i}"), e);
        }
        if let Some(c) = call.filter(|c| c.error.is_none()) {
            inference.push(c.elapsed);
            turns.push(work.latency.total);
            overhead.push(
                work.latency
                    .total
                    .saturating_sub(work.latency.model + work.latency.compute),
            );
        }
        iterations.push(serde_json::json!({
            "iteration": i,
            "outcome": outcome,
            "requests": calls.len(),
            "inference_ms": call.map(|c| c.elapsed.as_secs_f64() * 1e3),
            "turn_total_ms": work.latency.total.as_secs_f64() * 1e3,
            // `Usage` carries no "not supplied" state: zeros here may mean the endpoint sent no counts.
            "prompt_tokens": call.and_then(|c| c.tokens).map(|t| t.0),
            "completion_tokens": call.and_then(|c| c.tokens).map(|t| t.1),
            "error": error,
        }));
    }

    let id = |what: &str| format!("fxlive.{what}");
    report.unavailable(
        &id("time_to_first_response_event"),
        "the native provider port is private to chip-fx-native; the first response event is not observable from outside it without changing production code",
    );
    if inference.is_empty() {
        report.error(
            "fxlive",
            "no iteration produced a response; there is nothing to report",
        );
    } else {
        report.series(
            "L2",
            &id("inference_total"),
            "fxlive: total inference time (one request)",
            inference,
            "complete() entry to return, includes network and the model",
            None,
        );
        report.series(
            "L2",
            &id("turn_total"),
            "fxlive: total turn latency",
            turns,
            "run_work submission to outcome",
            None,
        );
        report.series(
            "L2",
            &id("chip_overhead"),
            "fxlive: Chip overhead in the turn",
            overhead,
            "turn total - model - execution, as the work loop measures them",
            None,
        );
    }
    report.scenario(
        "fxlive",
        serde_json::json!({
            "kind": "real-model",
            "iterations": cfg.iterations,
            "warmup": 0,
            "retries": 0,
            "max_requests": cfg.iterations,
        }),
    );
    report.real_model = serde_json::json!({
        "label": LABEL,
        "status": "run",
        "provider": PROVIDER_OPENAI_COMPATIBLE,
        "executor": "fx (native provider)",
        "model": cfg.model,
        "endpoint": endpoint_identity(&cfg.endpoint),
        "endpoint_type": cfg.endpoint_type,
        "credential_supplied": cfg.api_key.is_some(),
        "prompt_id": PROMPT_ID,
        "config": {
            "max_tokens": MAX_TOKENS, "temperature": 0.0, "timeout_s": TIMEOUT.as_secs(),
            "iterations": cfg.iterations, "warmup": 0, "retries": 0, "json_object_output": false,
        },
        "requests_made": requests,
        "time_to_first_response_event": null,
        "iterations": iterations,
    });
}
