//! Shadow-mode micro-model evaluation: `cargo bench -p chip-cli --bench micro_eval`.
//!
//! A measurement, not a test. It scores what a model says about the fixed snapshots of
//! `tests/fixtures/micro/fixture.json` and prints one JSON run record.
//!
//!   (no arguments)       ask the model configured by CHIP_MICRO_PROVIDER / CHIP_MICRO_MODEL /
//!                        CHIP_MICRO_ENDPOINT / CHIP_MICRO_API_KEY. With none configured, or none
//!                        answering, the run is `blocked`: no metrics, exit 3. A blocked run is never a
//!                        pass and never evidence about a model.
//!   --self-test MODE     score a scripted responder (oracle, abstain, adversarial) to test the harness.
//!                        Labelled `scripted_self_test`; says nothing about any model.
//!   --replay RECORD      score the replies of an earlier record again (reproducibility).
//!   --out PATH           also write the record to PATH (default target/micro-eval/record-<time>.json).
//!
//! Exit: 0 the run completed (or the self-test/replay did), 3 blocked, 2 usage.
//! Shadow mode grants a micro-model no authority; this measurement cannot show reduced larger-model
//! calls or better verified completion: that needs the controlled ablation of RIC-07.

use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chip_cli::micro::{self, ShadowIdentity};
use chip_cli::micro_eval::{
    self, ProviderResponder, Replay, RunContext, RunKind, Script, Scripted, record, run_cases,
};
use chip_cli::provider_selection::Selection;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn repository() -> serde_json::Value {
    let commit = git(&["rev-parse", "HEAD"]);
    let dirty = git(&["status", "--porcelain"]).map(|s| !s.is_empty());
    serde_json::json!({
        "git_commit": commit,
        "working_tree_dirty": dirty,
        "note": if commit.is_none() { Some("repository state unavailable") } else { None },
    })
}

fn usage(why: &str) -> ! {
    eprintln!(
        "error: {why}\nusage: micro_eval [--self-test oracle|abstain|adversarial | --replay RECORD] [--out PATH]"
    );
    std::process::exit(2)
}

fn main() {
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| a != "--bench")
        .collect();
    let (mut self_test, mut replay, mut out) = (None, None, None);
    let mut i = 0;
    while i < args.len() {
        let take = |i: &mut usize| {
            *i += 1;
            args.get(*i)
                .cloned()
                .unwrap_or_else(|| usage("a value is missing"))
        };
        match args[i].as_str() {
            "--self-test" => self_test = Some(take(&mut i)),
            "--replay" => replay = Some(take(&mut i)),
            "--out" => out = Some(take(&mut i)),
            other => usage(&format!("unexpected argument `{other}`")),
        }
        i += 1;
    }
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/",
        "tests/fixtures/micro/fixture.json"
    );
    let raw = std::fs::read_to_string(path).unwrap_or_else(|e| usage(&format!("{path}: {e}")));
    let fixture = micro_eval::load(&raw).unwrap_or_else(|e| usage(&e));
    if let Some(failed) = micro_eval::fixture_checks(&fixture)
        .iter()
        .find(|c| !c.passed)
    {
        usage(&format!(
            "fixture check failed: {}: {}",
            failed.name, failed.detail
        ));
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let timeout = std::env::var("CHIP_MICRO_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(micro::DEFAULT_TIMEOUT);

    let (kind, scripted, model, blocked_reason, results) = if let Some(mode) = self_test {
        let (script, name) = match mode.as_str() {
            "oracle" => (Script::Oracle, "oracle"),
            "abstain" => (Script::AbstainAll, "abstain"),
            "adversarial" => (Script::Adversarial, "adversarial"),
            other => usage(&format!("unknown self-test mode `{other}`")),
        };
        let rs = runtime.block_on(run_cases(&fixture, &Scripted(script)));
        (RunKind::ScriptedSelfTest, Some(name), None, None, rs)
    } else if let Some(file) = replay {
        let text =
            std::fs::read_to_string(&file).unwrap_or_else(|e| usage(&format!("{file}: {e}")));
        let record: serde_json::Value =
            serde_json::from_str(&text).unwrap_or_else(|e| usage(&format!("{file}: {e}")));
        let replies = record["replies"]
            .as_object()
            .unwrap_or_else(|| usage("the record has no `replies`"))
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
            .collect();
        let rs = runtime.block_on(run_cases(&fixture, &Replay(replies)));
        (RunKind::Replay, None, None, None, rs)
    } else {
        match micro::resolve(&Selection::default(), |name| std::env::var(name).ok()) {
            Err(e) => {
                let why = format!("no shadow model is configured ({e}); nothing was asked");
                finish(micro_eval::blocked(&fixture, why, repository()), out, true);
                unreachable!()
            }
            Ok(config) => {
                let identity = ShadowIdentity {
                    provider: config.provider.clone(),
                    model: config.model.to_string(),
                    endpoint: chip_cli::provider_selection::endpoint_identity(&config.endpoint),
                };
                let provider = fx_provider_http::HttpProvider::new(config)
                    .unwrap_or_else(|e| usage(&format!("the shadow provider is unusable ({e})")));
                let responder = ProviderResponder {
                    provider: Arc::new(provider),
                    model: identity.model.clone(),
                    timeout,
                };
                let rs = runtime.block_on(run_cases(&fixture, &responder));
                if rs.iter().all(|r| r.attempt.reply.is_none()) {
                    let first = rs.iter().find_map(|r| match &r.attempt.nomination {
                        micro::Nomination::ProviderFailed(e) => Some(e.clone()),
                        _ => None,
                    });
                    let why = format!(
                        "no case received a reply from {} {} at {} ({}); no metrics are reported",
                        identity.provider,
                        identity.model,
                        identity.endpoint,
                        first.unwrap_or_else(|| "timed out".into())
                    );
                    finish(micro_eval::blocked(&fixture, why, repository()), out, true);
                    unreachable!()
                }
                (RunKind::Completed, None, Some(identity), None, rs)
            }
        }
    };
    let ctx = RunContext {
        kind,
        blocked_reason,
        model,
        timeout,
        repository: repository(),
        scripted,
        fixture: &fixture,
    };
    finish(record(&ctx, &results), out, false);
}

/// Prints the record, writes it, and exits (3 when blocked).
fn finish(record: serde_json::Value, out: Option<String>, blocked: bool) {
    let text = serde_json::to_string_pretty(&record).unwrap();
    println!("{text}");
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = out.unwrap_or_else(|| {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/micro-eval");
        let _ = std::fs::create_dir_all(dir);
        format!("{dir}/record-{stamp}.json")
    });
    match std::fs::write(&path, &text) {
        Ok(()) => eprintln!("wrote {path}"),
        Err(e) => eprintln!("could not write {path}: {e}"),
    }
    if blocked {
        eprintln!(
            "BLOCKED: {}. Not a pass; no metrics reported.",
            record["blocked_reason"].as_str().unwrap_or("")
        );
        std::process::exit(3);
    }
}
